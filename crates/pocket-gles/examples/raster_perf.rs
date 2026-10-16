use pocket_gles::raster::*;
use std::{hint::black_box, time::Instant};
fn main() {
    for size in [2, 8, 64] {
        let n = if size <= 8 { 100000 } else { 3000 };
        let mut t = RenderTarget::new(size, size);
        let mut state = PipelineState::default();
        state.viewport = (0, 0, size as i32, size as i32);
        let tri = [
            Vertex {
                pos: [-1., -1., 0., 1.],
                ..Default::default()
            },
            Vertex {
                pos: [1., -1., 0., 1.],
                ..Default::default()
            },
            Vertex {
                pos: [-1., 1., 0., 1.],
                ..Default::default()
            },
        ];
        let sample: SampleFn<'_> = black_box(&(|_, _, _| None) as SampleFn<'_>);
        let stages = black_box(&[] as &[TextureStage]);
        let mut times = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            for _ in 0..n {
                black_box(draw_triangle(
                    black_box(&mut t),
                    black_box(&state),
                    sample,
                    stages,
                    black_box(tri),
                ));
            }
            times.push(start.elapsed());
        }
        times.sort();
        println!("{} {} {:?}", size, n, times[3]);
    }
}
