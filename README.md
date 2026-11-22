## Circular moving-window "percent of class" filter for a single-band Byte raster.

For every cell, the output is the percentage (0-100, rounded half up) of data cells within
`radius` whose value is one of `classes`. The window is clipped to the raster, nodata cells
are left out of the denominator, and the output is 255 wherever the input cell is nodata.

How it works:
- The disc is a stack of horizontal runs, one per row offset. With per-row prefix sums,
  each run costs one subtraction, so a cell costs about 2r+1 operations instead of about r².
- Each tile streams its input top to bottom through a ring buffer of 2r + ROW_BLOCK prefix
  rows, so memory per thread is set by the radius, not by the tile height.
- ROW_BLOCK output rows are computed together. Each input row is pulled from memory once and
  added into every output row it reaches while it is still in cache. That cuts memory
  traffic, which is what limits the inner loop.
- The valid and target counts are packed as two 16-bit lanes of one u32, in both the prefix
  sums and the hot accumulator, so one u32 add updates both counts. Every few rows, before
  a lane could overflow, the packed accumulator is moved into separate 32-bit totals.


GDAL version:

```
GDAL 3.11.5 "Eganville", released 2025/10/29
```

Warp to web mercator:

```sh
gdalwarp -t_srs EPSG:3857 -r near -srcnodata 255 -dstnodata 255 \
  -co COMPRESS=ZSTD rasters/[INPUT].tif rasters/[OUTPUT].tif
```

Colorize to RGBA

```sh
gdaldem color-relief -alpha rasters/[INPUT].tif ramp.txt rasters/[OUTPUT].tif
```

Tile as webp

```sh
gdal_translate -of MBTILES -co TILE_FORMAT=WEBP -co QUALITY=90 \
  -co ZOOM_LEVEL_STRATEGY=UPPER rasters/[INPUT].tif rasters/[OUTPUT].mbtiles

```sh
gdaladdo -r average rasters/[OUTPUT].mbtiles 2 4 8 16 32 64 128 256
```

Tile as pmtiles

```sh
docker compose run --rm pmtiles convert rasters/[INPUT].mbtiles rasters/[OUTPUT].pmtiles
```
