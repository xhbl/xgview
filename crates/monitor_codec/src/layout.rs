//! Which shape a decoder's colour is in, and how long one of its rows is.
//!
//! `YUV420Flexible` is what the Android decoder is asked for, and it means what
//! it says: the codec picks. Two shapes are allowed - interleaved, one plane of
//! U and V pairs after the luma; and planar, two planes of their own, half as
//! wide as the picture. `color-format` answers `YUV420Flexible` for both, so it
//! cannot be asked which one came back.
//!
//! What can be asked is the size of the buffer the codec handed over:
//!
//! ```text
//! interleaved: stride*slice_height + stride*chroma_height      (padding included)
//! planar:      stride*height       + 2*chroma_stride*chroma_height
//! ```
//!
//! A 640x480 stream here comes as 491520 bytes one session - interleaved, the
//! luma padded to 512 rows - and 460800 the next, which is exactly
//! `640*480 + 320*240 + 320*240` and nothing else: planar. Read as the other
//! shape the picture is sharp and green-magenta striped, with the luma where it
//! should be and the colour of a picture that is not there.
//!
//! When the luma is not padded the two shapes come to exactly the same size, and
//! there is nothing left to tell them apart. The answer is then the interleaved
//! one, because that is the shape the decoder is now *asked* for by name
//! (`COLOR_FormatYUV420SemiPlanar`): a codec that took the name is right, and one
//! that refused it and chose planar with no padding is the one case this cannot
//! see.

/// How a session's colour is laid out in the buffers the codec hands over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One interleaved plane, one chroma row per two luma rows (NV12).
    SemiPlanar,
    /// Two planes of their own, half as wide as the picture (I420).
    Planar,
    /// Neither fits the buffer, so nothing is drawn from it.
    Unknown,
}

/// Which shape a buffer of this size holds, and how long a colour row is.
///
/// `width` and `height` are the picture's, `stride` and `slice_height` the
/// codec's, and `capacity` the whole buffer as it was handed over.
pub fn shape_of(
    width: usize,
    height: usize,
    stride: usize,
    slice_height: usize,
    capacity: usize,
) -> (Layout, usize) {
    let chroma_height = height.div_ceil(2);
    let natural = width / 2;
    let semi = stride * slice_height + stride * chroma_height;
    let planar = stride * height + 2 * natural * chroma_height;
    let layout = if semi <= capacity {
        Layout::SemiPlanar
    } else if planar <= capacity {
        Layout::Planar
    } else {
        Layout::Unknown
    };
    // A colour row of a planar picture is half the picture by definition, and
    // whatever the codec padded it to in practice: what the buffer has left over
    // is the answer. An interleaved picture's rows are the luma's own length, and
    // the bytes between the luma and the colour there are padding, not a longer
    // row - which is why this is only asked of the planar shape.
    let chroma_stride = if layout == Layout::Planar {
        let colour_bytes = capacity.saturating_sub(stride * height);
        let derived = colour_bytes / 2 / chroma_height.max(1);
        if derived > 0 && derived <= stride { derived } else { natural }
    } else {
        natural
    };
    (layout, chroma_stride)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two shapes this box really hands over, on the same 640x480 stream.
    #[test]
    fn the_shape_is_read_from_the_size_of_the_buffer() {
        // Interleaved, luma padded to 512 rows: the colour starts at row 512.
        assert_eq!(shape_of(640, 480, 640, 512, 491_520), (Layout::SemiPlanar, 320));
        // Planar, no padding: 640*480 + 320*240 + 320*240. The `slice-height`
        // the codec reported (600) does not fit in that buffer at all.
        assert_eq!(shape_of(640, 480, 640, 600, 460_800), (Layout::Planar, 320));
    }

    /// When the luma is not padded the two shapes are the same size, and the
    /// interleaved one is the answer - it is the shape the decoder is asked for.
    #[test]
    fn the_shapes_that_are_the_same_size_answer_the_one_that_was_asked_for() {
        assert_eq!(shape_of(640, 480, 640, 480, 460_800), (Layout::SemiPlanar, 320));
    }

    /// A colour row is read at whatever length the codec padded it to - when the
    /// shape itself can still be told apart. A planar buffer whose rows are
    /// padded to 384 comes to the same size as an interleaved one with padded
    /// luma, and that pair this cannot separate; the `slice-height` here leaves
    /// no room for an interleaved plane, so it can.
    #[test]
    fn a_padded_colour_row_is_read_at_its_own_length() {
        let capacity = 640 * 480 + 2 * 384 * 240;
        assert_eq!(shape_of(640, 480, 640, 600, capacity), (Layout::Planar, 384));
        // And the row of an interleaved picture is the luma's own length.
        assert_eq!(shape_of(640, 480, 640, 512, 491_520), (Layout::SemiPlanar, 320));
    }

    /// A buffer too small for either shape is not drawn at all.
    #[test]
    fn a_buffer_that_fits_neither_shape_is_given_up() {
        assert_eq!(shape_of(640, 480, 640, 512, 300_000), (Layout::Unknown, 320));
    }

    /// The row length never comes out as nonsense, whatever the arithmetic says.
    #[test]
    fn the_row_length_falls_back_to_half_the_picture() {
        // A buffer with nothing left over for colour at all.
        let (_, chroma) = shape_of(640, 480, 640, 480, 640 * 480);
        assert_eq!(chroma, 320);
    }
}
