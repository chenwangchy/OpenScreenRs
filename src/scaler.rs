//! Scales a captured desktop into the frame that is streamed.
//!
//! A true area average: every destination pixel is the weighted mean of exactly the source area
//! it covers, with the weights in 1/256ths so the whole thing stays in integers and threads
//! cheaply. The first version rounded the covered ranges to whole pixels, which for a 2560 to
//! 1728 scale meant some destination pixels averaged one source pixel and their neighbours
//! averaged two: that is where the soft, slightly stepped text came from.

const MAX_SCALE_THREADS: usize = 6;
/// Weights are in 1/256ths, so a weighted sum shifts back down by 16 bits.
const WEIGHT_ONE: u32 = 256;

/// Copies `dst_pixels(dst_stride)` from the source that lives in `pixels`.
///
/// Requires `pixels` to hold `height * stride` bytes of BGRA and `out` to hold
/// `dst_height * dst_width * 4` bytes.
pub fn scale(
    pixels: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    out: &mut [u8],
    dst_width: usize,
    dst_height: usize,
) {
    if width == dst_width && height == dst_height {
        for y in 0..dst_height {
            let source = &pixels[y * stride..y * stride + dst_width * 4];
            out[y * dst_width * 4..(y + 1) * dst_width * 4].copy_from_slice(source);
        }

        return;
    }

    let columns = axis_taps(width, dst_width);
    let rows = axis_taps(height, dst_height);

    // Scaling is memory bound, so a handful of threads already saturates it. Spawning one
    // thread per core for every frame just adds scheduler churn: measured on a 20 thread
    // machine, 20 threads per frame was slower than 6.
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .min(MAX_SCALE_THREADS)
        .min(dst_height.max(1));
    let rows_per_thread = dst_height.div_ceil(threads).max(1);
    let bytes_per_row = dst_width * 4;

    std::thread::scope(|scope| {
        for (index, band) in out
            .chunks_mut(rows_per_thread * bytes_per_row)
            .enumerate()
        {
            let columns = &columns;
            let rows = &rows;
            let first_row = index * rows_per_thread;

            scope.spawn(move || {
                for (offset, row) in band.chunks_mut(bytes_per_row).enumerate() {
                    let row_taps = &rows[first_row + offset];

                    for (x, column_taps) in columns.iter().enumerate() {
                        let mut blue = 0u32;
                        let mut green = 0u32;
                        let mut red = 0u32;

                        for (source_y, weight_y) in row_taps {
                            let base = source_y * stride;

                            for (source_x, weight_x) in column_taps {
                                let pixel = base + source_x * 4;
                                let weight = weight_y * weight_x;

                                // One slice, one bounds check, three channels. Walking the
                                // bytes individually tripled the checks in the hot loop.
                                let sample = &pixels[pixel..pixel + 3];

                                blue += sample[0] as u32 * weight;
                                green += sample[1] as u32 * weight;
                                red += sample[2] as u32 * weight;
                            }
                        }

                        let target = &mut row[x * 4..x * 4 + 4];
                        target[0] = ((blue + 32768) >> 16) as u8;
                        target[1] = ((green + 32768) >> 16) as u8;
                        target[2] = ((red + 32768) >> 16) as u8;
                        target[3] = 255;
                    }
                }
            });
        }
    });
}

/// For every destination index, the source indices it covers and how much of each.
///
/// The weights of one index always add up to [`WEIGHT_ONE`]; the rounding error is taken out of
/// the widest tap so that flat areas keep their brightness.
fn axis_taps(source: usize, target: usize) -> Vec<Vec<(usize, u32)>> {
    let step = source as f64 / target as f64;
    let mut taps = Vec::with_capacity(target);

    for index in 0..target {
        let start = index as f64 * step;
        let end = ((index + 1) as f64 * step).min(source as f64);
        let first = (start.floor() as usize).min(source.saturating_sub(1));
        let last = (end.ceil() as usize).clamp(first + 1, source);

        let mut ranges: Vec<(usize, u32)> = Vec::with_capacity(last - first + 1);
        let mut total = 0u32;

        for source_index in first..last {
            let covered = end.min((source_index + 1) as f64) - start.max(source_index as f64);

            if covered <= 0.0 {
                continue;
            }

            let weight = ((covered / step) * WEIGHT_ONE as f64).round() as u32;

            if weight == 0 {
                continue;
            }

            total += weight;
            ranges.push((source_index, weight));
        }

        if ranges.is_empty() {
            ranges.push((first, WEIGHT_ONE));
        } else if total != WEIGHT_ONE {
            let widest = ranges
                .iter()
                .enumerate()
                .max_by_key(|(_, (_, weight))| *weight)
                .map(|(index, _)| index)
                .unwrap();
            let adjusted = ranges[widest].1 as i64 + WEIGHT_ONE as i64 - total as i64;
            ranges[widest].1 = adjusted.clamp(1, WEIGHT_ONE as i64) as u32;
        }

        taps.push(ranges);
    }

    taps
}

/// The frame size for a height, keeping the aspect ratio of the screen.
///
/// A 16:10 desktop streamed as "1080p" becomes 1728x1080 instead of being stretched into
/// 1920x1080, and it is never scaled up.
pub fn target_size(width: usize, height: usize, target_height: usize) -> (usize, usize) {
    if width == 0 || height == 0 {
        return (target_height * 16 / 9, target_height);
    }

    let frame_height = target_height.min(height);
    let frame_width = (width as f64 * frame_height as f64 / height as f64).round() as usize;

    (frame_width.max(1), frame_height.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_always_add_up_to_one() {
        for (source, target) in [(2560, 1728), (1600, 1080), (100, 7), (7, 3), (5, 5)] {
            let taps = axis_taps(source, target);

            assert_eq!(taps.len(), target);

            for (index, ranges) in taps.iter().enumerate() {
                let total: u32 = ranges.iter().map(|(_, weight)| weight).sum();
                assert_eq!(total, WEIGHT_ONE, "index {index} of {source}->{target} sums to {total}");
                assert!(ranges.iter().all(|(source_index, _)| *source_index < source));
            }
        }
    }

    #[test]
    fn a_flat_area_stays_exactly_the_same_colour() {
        // A 4x4 white source scaled to 2x2 must be exactly white, not 254 due to rounding.
        let pixels = vec![255u8; 4 * 4 * 4];
        let mut out = vec![0u8; 2 * 2 * 4];
        scale(&pixels, 4, 4, 16, &mut out, 2, 2);

        assert!(out.chunks_exact(4).all(|pixel| pixel[0] == 255 && pixel[1] == 255 && pixel[2] == 255));
    }

    #[test]
    fn averaging_uses_the_whole_covered_area() {
        // Left half black, right half white, scaled to one pixel: the answer is half grey.
        let mut pixels = vec![0u8; 4 * 4 * 4];

        for y in 0..4 {
            for x in 2..4 {
                let offset = (y * 4 + x) * 4;
                pixels[offset] = 255;
                pixels[offset + 1] = 255;
                pixels[offset + 2] = 255;
            }
        }

        let mut out = vec![0u8; 4];
        scale(&pixels, 4, 4, 16, &mut out, 1, 1);

        assert_eq!(out[0], 128, "half covered should be mid grey, got {}", out[0]);
    }
}
