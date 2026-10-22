//! Optional GPU acceleration for exact squared-L2 search (cargo feature `gpu`).
//!
//! The GPU computes f32 distances for every live vector; the best
//! `4 * k + 32` candidates are then re-scored on the CPU in f64 so the final
//! distances and ordering are the same as [`Database::search`], except for
//! near-ties closer than f32 rounding error. If no adapter exists or a GPU
//! call fails, [`GpuExactSearcher::search_or_cpu`] falls back to the CPU.
//! Nothing here persists data.

use crate::{Database, Error, Result, SearchHit, current_time_ms};
use std::sync::mpsc;
use wgpu::util::DeviceExt;

const SHADER: &str = r#"
struct Params { dims: u32, count: u32, _a: u32, _b: u32 };
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> query: array<f32>;
@group(0) @binding(2) var<storage, read> vectors: array<f32>;
@group(0) @binding(3) var<storage, read_write> distances: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let row = id.x;
    if (row >= params.count) { return; }
    var sum = 0.0;
    let base = row * params.dims;
    for (var i = 0u; i < params.dims; i = i + 1u) {
        let d = vectors[base + i] - query[i];
        sum = sum + d * d;
    }
    distances[row] = sum;
}
"#;

/// Rows scored per dispatch; bounds GPU buffer sizes.
const CHUNK_FLOATS: usize = 16 * 1024 * 1024;

/// A compute device for exact search. Construct with [`GpuExactSearcher::new`].
pub struct GpuExactSearcher {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    adapter_name: String,
}

impl GpuExactSearcher {
    /// Returns `None` when no compute-capable adapter is available.
    pub fn new() -> Option<Self> {
        pollster::block_on(Self::new_async())
    }

    async fn new_async() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .ok()?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vlite-l2"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("vlite-l2"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Some(Self {
            device,
            queue,
            pipeline,
            adapter_name,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Exact squared-L2 search of the default collection on the GPU.
    pub fn search(&self, database: &Database, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        database.ensure_healthy()?;
        let state = database.state(0);
        crate::validate_vector(state.dimensions, query)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let now = current_time_ms();
        let live: Vec<(u64, &[f32])> = state
            .records
            .iter()
            .filter(|(id, _)| state.expiries.get(id).is_none_or(|expires| *expires > now))
            .map(|(&id, record)| (id, record.vector.as_slice()))
            .collect();
        if live.is_empty() {
            return Ok(Vec::new());
        }
        let dims = state.dimensions;
        let rows_per_chunk = (CHUNK_FLOATS / dims).max(1);
        let mut scored: Vec<(f32, u64)> = Vec::with_capacity(live.len());
        for chunk in live.chunks(rows_per_chunk) {
            let flat: Vec<f32> = chunk.iter().flat_map(|(_, v)| v.iter().copied()).collect();
            let distances = self.distances(query, &flat, dims, chunk.len())?;
            scored.extend(distances.into_iter().zip(chunk.iter().map(|(id, _)| *id)));
        }
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let keep = k.saturating_mul(4).saturating_add(32).min(scored.len());
        let mut hits: Vec<SearchHit> = scored[..keep]
            .iter()
            .map(|&(_, id)| {
                let record = &state.records[&id];
                let distance = record
                    .vector
                    .iter()
                    .zip(query)
                    .map(|(&a, &b)| {
                        let d = f64::from(a) - f64::from(b);
                        d * d
                    })
                    .sum();
                SearchHit { id, distance }
            })
            .collect();
        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
        hits.truncate(k);
        Ok(hits)
    }

    /// GPU search, or the CPU path when `searcher` is `None` or the GPU fails.
    pub fn search_or_cpu(
        searcher: Option<&Self>,
        database: &Database,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        match searcher.map(|searcher| searcher.search(database, query, k)) {
            Some(Ok(hits)) => Ok(hits),
            Some(Err(Error::GpuUnavailable(_))) | None => database.search(query, k),
            Some(Err(error)) => Err(error),
        }
    }

    fn distances(&self, query: &[f32], flat: &[f32], dims: usize, rows: usize) -> Result<Vec<f32>> {
        let params = [dims as u32, rows as u32, 0, 0];
        let params_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let query_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(query),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let vectors_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(flat),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let size = (rows * 4) as u64;
        let output = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = self.pipeline.get_bind_group_layout(0);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: query_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: vectors_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: output.as_entire_binding(),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            let groups = (rows as u32).div_ceil(64);
            // Stay within the 65535 per-dimension dispatch limit for huge chunks.
            if groups > 65_535 {
                return Err(Error::GpuUnavailable("chunk exceeds dispatch limit"));
            }
            pass.dispatch_workgroups(groups, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, size);
        self.queue.submit(Some(encoder.finish()));
        let (sender, receiver) = mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|_| Error::GpuUnavailable("device poll failed"))?;
        receiver
            .recv()
            .map_err(|_| Error::GpuUnavailable("readback channel closed"))?
            .map_err(|_| Error::GpuUnavailable("readback mapping failed"))?;
        let data = readback
            .slice(..)
            .get_mapped_range()
            .map_err(|_| Error::GpuUnavailable("readback range failed"))?;
        let distances = bytemuck::cast_slice::<u8, f32>(&data).to_vec();
        drop(data);
        readback.unmap();
        Ok(distances)
    }
}
