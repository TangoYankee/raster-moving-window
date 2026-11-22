use gdal::raster::{Buffer, GdalDataType, RasterCreationOptions};
use gdal::{Dataset, DriverManager};
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{SyncSender, sync_channel};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

const BLOCK: usize = 512;
const TILE_W: usize = 4 * BLOCK;
const MAX_TILE_H: usize = 16 * BLOCK;

const ROW_BLOCK: usize = 8;

const READ_ROWS: usize = 256;
const OUT_NODATA: u8 = 255;

const VALID: u32 = 1 << 16;
const HIT: u32 = 1;
const LO16: u32 = 0xFFFF;
const MAX_COLS: usize = (1 << 16) - 1;

struct Window<'a> {
    width: usize,
    height: usize,
    radius: usize,
    half_widths: &'a [usize],
    lut: &'a [u32; 256],
}

#[derive(Clone, Copy, Debug)]
struct Tile {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
}

struct Chunk {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    data: Vec<u8>,
}

#[derive(Default)]
struct Scratch {
    raw: Vec<u8>,
    ring: Vec<u32>,
    acc: Vec<u32>,
    valid: Vec<u32>,
    hits: Vec<u32>,
}

pub fn class_percent_moving_window(
    input: &str,
    output: &str,
    radius: f64,
    classes: &[u8],
) -> Result<()> {
    let src = Dataset::open(input)?;
    let band = src.rasterband(1)?;
    if band.band_type() != GdalDataType::UInt8 {
        return Err(format!("expected a Byte band, found {:?}", band.band_type()).into());
    }
    if !(radius.is_finite() && radius >= 0.0) {
        return Err(format!("radius must be a non-negative number, got {radius}").into());
    }
    let (width, height) = src.raster_size();
    let r = (radius / pixel_size(&src.geo_transform()?)?).round() as usize;
    if TILE_W + 2 * r > MAX_COLS {
        return Err(format!(
            "radius of {r} px is too large (max {})",
            (MAX_COLS - TILE_W) / 2
        )
        .into());
    }

    let nodata = band
        .no_data_value()
        .filter(|v| v.fract() == 0.0 && (0.0..=255.0).contains(v))
        .map(|v| v as u8);

    let lut = class_lut(classes, nodata);
    let half_widths = disc_half_widths(r);
    let win = Window {
        width,
        height,
        radius: r,
        half_widths: &half_widths,
        lut: &lut,
    };

    let mut out = create_output(&src, output)?;
    let tile_h = tile_height(width, height, rayon::current_num_threads());
    run(input, &win, tiles(width, height, TILE_W, tile_h), &out)?;
    out.flush_cache()?;
    Ok(())
}

fn run(input: &str, win: &Window, tiles: Vec<Tile>, out: &Dataset) -> Result<()> {
    let cancel = &AtomicBool::new(false);
    let (tx, rx) = sync_channel::<Result<Chunk>>(2 * rayon::current_num_threads());

    std::thread::scope(|s| {
        s.spawn(move || {
            tiles.into_par_iter().for_each_init(
                || (None, Scratch::default()),
                |(ds, scratch), tile| {
                    if cancel.load(Relaxed) {
                        return;
                    }
                    if let Err(e) = gdal_tile(input, ds, scratch, tile, win, &tx, cancel) {
                        cancel.store(true, Relaxed);
                        let _ = tx.send(Err(e));
                    }
                },
            );
        });

        let rx = rx;
        let result = (|| -> Result<()> {
            let mut band = out.rasterband(1)?;
            for msg in &rx {
                let c = msg?;
                let mut buf = Buffer::new((c.w, c.h), c.data);
                band.write((c.x0 as isize, c.y0 as isize), (c.w, c.h), &mut buf)?;
            }
            Ok(())
        })();
        if result.is_err() {
            cancel.store(true, Relaxed);
        }
        drop(rx);
        result
    })
}

fn gdal_tile(
    input: &str,
    ds: &mut Option<Dataset>,
    scratch: &mut Scratch,
    tile: Tile,
    win: &Window,
    tx: &SyncSender<Result<Chunk>>,
    cancel: &AtomicBool,
) -> Result<()> {
    if ds.is_none() {
        *ds = Some(Dataset::open(input)?);
    }
    let band = ds.as_ref().unwrap().rasterband(1)?;
    compute_tile(
        tile,
        win,
        scratch,
        |x, y, cols, rows, buf| {
            let (pos, size) = ((x as isize, y as isize), (cols, rows));
            Ok(band.read_into_slice::<u8>(pos, size, size, buf, None)?)
        },
        |chunk| !cancel.load(Relaxed) && tx.send(Ok(chunk)).is_ok(),
    )
}

fn compute_tile(
    tile: Tile,
    win: &Window,
    s: &mut Scratch,
    mut read: impl FnMut(usize, usize, usize, usize, &mut [u8]) -> Result<()>,
    mut emit: impl FnMut(Chunk) -> bool,
) -> Result<()> {
    let Window {
        width,
        height,
        radius: r,
        half_widths: hw,
        lut,
    } = *win;
    let Tile {
        x0,
        y0,
        w: tw,
        h: th,
    } = tile;
    let y_end = y0 + th;

    let in_x0 = x0.saturating_sub(r);
    let cols = (x0 + tw + r).min(width) - in_x0;
    let off = x0 - in_x0;
    let stride = cols + 1;
    let cap = 2 * r + ROW_BLOCK;

    let rows_per_flush = MAX_COLS / (2 * r + 1).min(cols);

    s.ring.resize(cap * stride, 0);
    s.acc.resize(ROW_BLOCK * tw, 0);
    s.valid.resize(ROW_BLOCK * tw, 0);
    s.hits.resize(ROW_BLOCK * tw, 0);

    let mut loaded = y0.saturating_sub(r);
    let mut chunk = Chunk {
        x0,
        y0,
        w: tw,
        h: 0,
        data: Vec::with_capacity(BLOCK.min(th) * tw),
    };

    for by in (y0..y_end).step_by(ROW_BLOCK) {
        let nk = ROW_BLOCK.min(y_end - by);
        let first = by.saturating_sub(r);
        let last = (by + nk - 1 + r).min(height - 1);

        while loaded <= last {
            let n = (last + 1 - loaded).min(READ_ROWS);
            s.raw.resize(n * cols, 0);
            read(in_x0, loaded, cols, n, &mut s.raw)?;
            for (k, src) in s.raw.chunks_exact(cols).enumerate() {
                let dst = &mut s.ring[((loaded + k) % cap) * stride..][..stride];
                dst[0] = 0;
                let mut sum = 0;
                for (d, &v) in dst[1..].iter_mut().zip(src) {
                    sum += lut[v as usize];
                    *d = sum;
                }
            }
            loaded += n;
        }

        let acc = &mut s.acc[..nk * tw];
        let (valid, hits) = (&mut s.valid[..nk * tw], &mut s.hits[..nk * tw]);
        valid.fill(0);
        hits.fill(0);
        let mut pending = 0;

        for gr in first..=last {
            let prow = &s.ring[(gr % cap) * stride..][..stride];
            for k in 0..nk {
                let dy = gr.abs_diff(by + k);
                if dy <= r {
                    add_runs(prow, hw[dy], off, &mut acc[k * tw..][..tw]);
                }
            }
            pending += 1;
            if pending == rows_per_flush || gr == last {
                for ((a, v), h) in acc.iter_mut().zip(&mut *valid).zip(&mut *hits) {
                    *v += *a >> 16;
                    *h += *a & LO16;
                    *a = 0;
                }
                pending = 0;
            }
        }

        for k in 0..nk {
            let gy = by + k;
            let prow = &s.ring[(gy % cap) * stride..][..stride];
            let (v, h) = (&valid[k * tw..][..tw], &hits[k * tw..][..tw]);
            chunk.data.extend((0..tw).map(|i| {
                let c = off + i;
                let center_valid = (prow[c + 1] - prow[c]) & VALID != 0;
                if center_valid {
                    percent(h[i], v[i])
                } else {
                    OUT_NODATA
                }
            }));
            chunk.h += 1;

            if chunk.h == BLOCK || gy + 1 == y_end {
                let rest = y_end - (gy + 1);
                let next = Chunk {
                    x0,
                    y0: gy + 1,
                    w: tw,
                    h: 0,
                    data: Vec::with_capacity(BLOCK.min(rest) * tw),
                };
                if !emit(std::mem::replace(&mut chunk, next)) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

#[inline]
fn add_runs(prow: &[u32], w: usize, off: usize, acc: &mut [u32]) {
    let cols = prow.len() - 1;
    let tw = acc.len();
    let start = w.saturating_sub(off).min(tw);
    let end = cols.saturating_sub(off + w).clamp(start, tw);

    for i in (0..start).chain(end..tw) {
        let c = off + i;
        acc[i] += prow[(c + w + 1).min(cols)] - prow[c.saturating_sub(w)];
    }

    if start < end {
        let hi = &prow[off + w + 1 + start..off + w + 1 + end];
        let lo = &prow[off + start - w..off + end - w];
        for ((a, &h), &l) in acc[start..end].iter_mut().zip(hi).zip(lo) {
            *a += h - l;
        }
    }
}

#[inline]
fn percent(hits: u32, valid: u32) -> u8 {
    let (h, v) = (hits as u64, valid as u64);
    ((200 * h + v) / (2 * v)) as u8
}

fn class_lut(classes: &[u8], nodata: Option<u8>) -> [u32; 256] {
    let mut lut = [VALID; 256];
    for &c in classes {
        lut[c as usize] |= HIT;
    }
    if let Some(nd) = nodata {
        lut[nd as usize] = 0;
    }
    lut
}

fn disc_half_widths(r: usize) -> Vec<usize> {
    (0..=r).map(|dy| (r * r - dy * dy).isqrt()).collect()
}

fn pixel_size(gt: &[f64; 6]) -> Result<f64> {
    let (px, py) = (gt[1].abs(), gt[5].abs());
    if gt[2] != 0.0 || gt[4] != 0.0 || (px - py).abs() > 1e-9 * px.max(py) || px == 0.0 {
        return Err(format!("need square, north-up pixels; geotransform is {gt:?}").into());
    }
    Ok(px)
}

fn tile_height(width: usize, height: usize, threads: usize) -> usize {
    let tile_cols = width.div_ceil(TILE_W);
    let tile_rows = (4 * threads).div_ceil(tile_cols);
    height
        .div_ceil(tile_rows)
        .next_multiple_of(BLOCK)
        .clamp(BLOCK, MAX_TILE_H)
}

fn tiles(width: usize, height: usize, tile_w: usize, tile_h: usize) -> Vec<Tile> {
    (0..height)
        .step_by(tile_h)
        .flat_map(|y0| {
            (0..width).step_by(tile_w).map(move |x0| Tile {
                x0,
                y0,
                w: tile_w.min(width - x0),
                h: tile_h.min(height - y0),
            })
        })
        .collect()
}

fn create_output(src: &Dataset, path: &str) -> Result<Dataset> {
    let driver = DriverManager::get_driver_by_name("GTiff")?;
    let opts = RasterCreationOptions::from_iter([
        "TILED=YES",
        "BLOCKXSIZE=512",
        "BLOCKYSIZE=512",
        "COMPRESS=ZSTD",
        "PREDICTOR=2",
        "BIGTIFF=YES",
    ]);
    let (w, h) = src.raster_size();
    let mut out = driver.create_with_band_type_with_options::<u8, _>(path, w, h, 1, &opts)?;
    out.set_geo_transform(&src.geo_transform()?)?;
    out.set_projection(&src.projection())?;
    out.rasterband(1)?
        .set_no_data_value(Some(OUT_NODATA as f64))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute_force(raw: &[u8], width: usize, height: usize, r: usize, lut: &[u32; 256]) -> Vec<u8> {
        let mut out = vec![OUT_NODATA; width * height];
        for y in 0..height {
            for x in 0..width {
                if lut[raw[y * width + x] as usize] & VALID == 0 {
                    continue;
                }
                let (mut valid, mut hits) = (0u32, 0u32);
                for yy in y.saturating_sub(r)..=(y + r).min(height - 1) {
                    for xx in x.saturating_sub(r)..=(x + r).min(width - 1) {
                        let (dx, dy) = (xx.abs_diff(x), yy.abs_diff(y));
                        if dx * dx + dy * dy <= r * r {
                            let v = lut[raw[yy * width + xx] as usize];
                            valid += v >> 16;
                            hits += v & LO16;
                        }
                    }
                }
                out[y * width + x] = percent(hits, valid);
            }
        }
        out
    }

    fn fast(
        raw: &[u8],
        width: usize,
        height: usize,
        r: usize,
        lut: &[u32; 256],
        tw: usize,
        th: usize,
    ) -> Vec<u8> {
        let hw = disc_half_widths(r);
        let win = Window {
            width,
            height,
            radius: r,
            half_widths: &hw,
            lut,
        };
        let mut out = vec![0u8; width * height];
        let mut s = Scratch::default();
        for t in tiles(width, height, tw, th) {
            let read = |x: usize, y: usize, c: usize, n: usize, buf: &mut [u8]| {
                for k in 0..n {
                    buf[k * c..][..c].copy_from_slice(&raw[(y + k) * width + x..][..c]);
                }
                Ok(())
            };
            compute_tile(t, &win, &mut s, read, |ch| {
                for row in 0..ch.h {
                    out[(ch.y0 + row) * width + ch.x0..][..ch.w]
                        .copy_from_slice(&ch.data[row * ch.w..][..ch.w]);
                }
                true
            })
            .unwrap();
        }
        out
    }

    fn random_raster(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x % 9) as u8
            })
            .collect()
    }

    #[test]
    fn matches_brute_force() {
        let (w, h) = (131, 97);
        let raw = random_raster(w * h, 0x9E37_79B9_7F4A_7C15);
        let lut = class_lut(&[1, 3, 4], Some(8));
        for r in [0, 1, 2, 3, 7, 20, 60, 200] {
            let expected = brute_force(&raw, w, h, r, &lut);
            for (tw, th) in [(w, h), (37, 53), (16, 5), (1, 1)] {
                assert_eq!(
                    fast(&raw, w, h, r, &lut, tw, th),
                    expected,
                    "r={r} tile={tw}x{th}"
                );
            }
        }
    }

    #[test]
    fn no_nodata_and_all_nodata() {
        let (w, h) = (40, 30);
        let raw = random_raster(w * h, 42);
        let lut = class_lut(&[2], None);
        assert_eq!(
            fast(&raw, w, h, 5, &lut, 16, 16),
            brute_force(&raw, w, h, 5, &lut)
        );
        let all_nd = vec![8u8; w * h];
        let lut = class_lut(&[2], Some(8));
        assert!(
            fast(&all_nd, w, h, 5, &lut, 16, 16)
                .iter()
                .all(|&v| v == OUT_NODATA)
        );
    }

    #[test]
    fn half_widths_are_exact() {
        for r in [0usize, 1, 5, 1312] {
            for (dy, &w) in disc_half_widths(r).iter().enumerate() {
                assert!(w * w + dy * dy <= r * r && (w + 1) * (w + 1) + dy * dy > r * r);
            }
        }
    }
}
