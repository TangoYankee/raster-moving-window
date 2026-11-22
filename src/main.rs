mod moving_window;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Res<()> {
    moving_window::class_percent_moving_window(
        "rasters/landcover_brooklyn_2021_6in.tif",
        "rasters/tree_grass_pct_656ft_6in_brooklyn.tif",
        656.0,
        &[1, 2],
    )?;
    Ok(())
}
