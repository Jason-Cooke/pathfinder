// pathfinder/renderer/src/builder.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Packs data onto the GPU.

use crate::command::{AlphaTileBatchPrimitive, FillBatchPrimitive, RenderCommand};
use crate::command::{SolidTileBatchPrimitive, TileObjectPrimitive};
use crate::concurrent::executor::Executor;
use crate::manager::{BuildOptions, PreparedRenderTransform, RenderCommandListener};
use crate::scene::{PathObject, Scene};
use crate::tile_map::DenseTileMap;
use crate::tiles::{self, TILE_HEIGHT, TILE_WIDTH, Tiler};
use crate::z_buffer::ZBuffer;
use pathfinder_geometry::line_segment::{LineSegment2F, LineSegmentU4, LineSegmentU8};
use pathfinder_geometry::rect::{RectF, RectI};
use pathfinder_geometry::util;
use pathfinder_geometry::vector::{Vector2F, Vector2I};
use pathfinder_simd::default::{F32x4, I32x4};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::u16;

pub(crate) struct SceneBuilder<'a> {
    scene: &'a Scene,

    transform: PreparedRenderTransform,
    options: BuildOptions,

    pub(crate) next_alpha_tile_index: AtomicUsize,
    pub(crate) z_buffer: ZBuffer,
    pub(crate) listener: &'a dyn RenderCommandListener,
}

impl<'a> SceneBuilder<'a> {
    pub(crate) fn new(
        scene: &'a Scene,
        transform: PreparedRenderTransform,
        options: &BuildOptions,
        listener: &'a dyn RenderCommandListener,
    ) -> SceneBuilder<'a> {
        let effective_view_box = scene.effective_view_box(options);
        SceneBuilder {
            scene,

            transform,
            options: *options,

            next_alpha_tile_index: AtomicUsize::new(0),
            z_buffer: ZBuffer::new(effective_view_box),
            listener,
        }
    }

    pub fn build<E>(&mut self, executor: &E) -> (SceneTiles, Duration) where E: Executor {
        let start_time = Instant::now();

        let bounding_quad = self.transform.bounding_quad();
        let path_count = self.scene.paths.len();
        self.listener.send(RenderCommand::Start { bounding_quad, path_count });

        self.listener.send(RenderCommand::AddPaintData(self.scene.build_paint_data()));

        let effective_view_box = self.scene.effective_view_box(&self.options);
        let alpha_tiles = executor.flatten_into_vector(path_count, |path_index| {
            self.build_path(path_index, effective_view_box, &self.scene)
        });

        let scene_tiles = self.finish_building(alpha_tiles);
        let build_time = Instant::now() - start_time;
        (scene_tiles, build_time)
    }

    fn build_path(
        &self,
        path_index: usize,
        view_box: RectF,
        scene: &Scene,
    ) -> Vec<AlphaTileBatchPrimitive> {
        let path_object = &scene.paths[path_index];
        let outline = scene.apply_render_options(path_object.outline(),
                                                 &self.transform,
                                                 &self.options);
        let paint_id = path_object.paint();
        let object_is_opaque = scene.paints[paint_id.0 as usize].is_opaque();

        let mut tiler = Tiler::new(self,
                                   &outline,
                                   view_box,
                                   path_index as u16,
                                   paint_id,
                                   object_is_opaque);

        tiler.generate_tiles();

        self.listener.send(RenderCommand::AddFills(tiler.built_object.fills));
        tiler.built_object.alpha_tiles
    }

    fn cull_alpha_tiles(&self, alpha_tiles: &mut Vec<AlphaTileBatchPrimitive>) {
        for alpha_tile in alpha_tiles {
            let alpha_tile_coords = alpha_tile.tile_coords();
            if self
                .z_buffer
                .test(alpha_tile_coords, alpha_tile.object_index as u32)
            {
                continue;
            }

            // FIXME(pcwalton): Clean this up.
            alpha_tile.tile_x_lo = 0xff;
            alpha_tile.tile_y_lo = 0xff;
            alpha_tile.tile_hi = 0xff;
        }
    }

    fn finish_building(&mut self, mut alpha_tiles: Vec<AlphaTileBatchPrimitive>) -> SceneTiles {
        self.listener.send(RenderCommand::FlushFills);
        self.cull_alpha_tiles(&mut alpha_tiles);
        SceneTiles::new(Arc::new(alpha_tiles), &self.z_buffer, &self.scene.paths)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TileStats {
    pub solid_tile_count: u32,
    pub alpha_tile_count: u32,
}

// Precomposed scenes

#[derive(Clone)]
pub(crate) struct SceneTiles {
    pub(crate) alpha: Arc<Vec<AlphaTileBatchPrimitive>>,
    pub(crate) solid: Arc<Vec<SolidTileBatchPrimitive>>,
}

impl SceneTiles {
    pub(crate) fn new(alpha_tiles: Arc<Vec<AlphaTileBatchPrimitive>>,
                      z_buffer: &ZBuffer,
                      paths: &[PathObject])
                      -> SceneTiles {
        SceneTiles {
            alpha: alpha_tiles,
            solid: Arc::new(z_buffer.build_solid_tiles(paths, 0..(paths.len() as u32))),
        }
    }

    pub(crate) fn send(&self, listener: &Box<dyn RenderCommandListener>) {
        if !self.solid.is_empty() {
            listener.send(RenderCommand::SolidTile(self.solid.clone()));
        }
        if !self.alpha.is_empty() {
            listener.send(RenderCommand::AlphaTile(self.alpha.clone()));
        }
    }
}

// Built objects

#[derive(Debug)]
pub(crate) struct BuiltObject {
    pub bounds: RectF,
    pub fills: Vec<FillBatchPrimitive>,
    pub alpha_tiles: Vec<AlphaTileBatchPrimitive>,
    pub tiles: DenseTileMap<TileObjectPrimitive>,
}

impl BuiltObject {
    pub(crate) fn new(bounds: RectF) -> BuiltObject {
        let tile_rect = tiles::round_rect_out_to_tile_bounds(bounds);
        let tiles = DenseTileMap::new(tile_rect);
        BuiltObject {
            bounds,
            fills: vec![],
            alpha_tiles: vec![],
            tiles,
        }
    }

    #[inline]
    pub(crate) fn tile_rect(&self) -> RectI {
        self.tiles.rect
    }

    fn add_fill(
        &mut self,
        builder: &SceneBuilder,
        segment: LineSegment2F,
        tile_coords: Vector2I,
    ) {
        debug!("add_fill({:?} ({:?}))", segment, tile_coords);

        // Ensure this fill is in bounds. If not, cull it.
        if self.tile_coords_to_local_index(tile_coords).is_none() {
            return;
        };

        debug_assert_eq!(TILE_WIDTH, TILE_HEIGHT);

        // Compute the upper left corner of the tile.
        let tile_size = F32x4::splat(TILE_WIDTH as f32);
        let tile_upper_left = tile_coords.to_f32().0.to_f32x4().xyxy() * tile_size;

        // Convert to 4.8 fixed point.
        let segment = (segment.0 - tile_upper_left) * F32x4::splat(256.0);
        let (min, max) = (F32x4::default(), F32x4::splat((TILE_WIDTH * 256 - 1) as f32));
        let segment = segment.clamp(min, max).to_i32x4();
        let (from_x, from_y, to_x, to_y) = (segment[0], segment[1], segment[2], segment[3]);

        // Cull degenerate fills.
        if from_x == to_x {
            debug!("... culling!");
            return;
        }

        // Allocate global tile if necessary.
        let alpha_tile_index = self.get_or_allocate_alpha_tile_index(builder, tile_coords);

        // Pack whole pixels.
        let mut px = (segment & I32x4::splat(0xf00)) >> I32x4::new(8, 4, 8, 4);
        px = px | px.yxwz();

        // Pack instance data.
        debug!("... OK, pushing");
        self.fills.push(FillBatchPrimitive {
            px: LineSegmentU4 { from: px[0] as u8, to: px[2] as u8 },
            subpx: LineSegmentU8 {
                from_x: from_x as u8,
                from_y: from_y as u8,
                to_x:   to_x   as u8,
                to_y:   to_y   as u8,
            },
            alpha_tile_index,
        });
    }

    fn get_or_allocate_alpha_tile_index(
        &mut self,
        builder: &SceneBuilder,
        tile_coords: Vector2I,
    ) -> u16 {
        let local_tile_index = self.tiles.coords_to_index_unchecked(tile_coords);
        let alpha_tile_index = self.tiles.data[local_tile_index].alpha_tile_index;
        if alpha_tile_index != !0 {
            return alpha_tile_index;
        }

        let alpha_tile_index = builder
            .next_alpha_tile_index
            .fetch_add(1, Ordering::Relaxed) as u16;
        self.tiles.data[local_tile_index].alpha_tile_index = alpha_tile_index;
        alpha_tile_index
    }

    pub(crate) fn add_active_fill(
        &mut self,
        builder: &SceneBuilder,
        left: f32,
        right: f32,
        mut winding: i32,
        tile_coords: Vector2I,
    ) {
        let tile_origin_y = (tile_coords.y() * TILE_HEIGHT as i32) as f32;
        let left = Vector2F::new(left, tile_origin_y);
        let right = Vector2F::new(right, tile_origin_y);

        let segment = if winding < 0 {
            LineSegment2F::new(left, right)
        } else {
            LineSegment2F::new(right, left)
        };

        debug!(
            "... emitting active fill {} -> {} winding {} @ tile {:?}",
            left.x(),
            right.x(),
            winding,
            tile_coords
        );

        while winding != 0 {
            self.add_fill(builder, segment, tile_coords);
            if winding < 0 {
                winding += 1
            } else {
                winding -= 1
            }
        }
    }

    pub(crate) fn generate_fill_primitives_for_line(
        &mut self,
        builder: &SceneBuilder,
        mut segment: LineSegment2F,
        tile_y: i32,
    ) {
        debug!(
            "... generate_fill_primitives_for_line(): segment={:?} tile_y={} ({}-{})",
            segment,
            tile_y,
            tile_y as f32 * TILE_HEIGHT as f32,
            (tile_y + 1) as f32 * TILE_HEIGHT as f32
        );

        let winding = segment.from_x() > segment.to_x();
        let (segment_left, segment_right) = if !winding {
            (segment.from_x(), segment.to_x())
        } else {
            (segment.to_x(), segment.from_x())
        };

        // FIXME(pcwalton): Optimize this.
        let segment_tile_left = f32::floor(segment_left) as i32 / TILE_WIDTH as i32;
        let segment_tile_right =
            util::alignup_i32(f32::ceil(segment_right) as i32, TILE_WIDTH as i32);
        debug!(
            "segment_tile_left={} segment_tile_right={} tile_rect={:?}",
            segment_tile_left,
            segment_tile_right,
            self.tile_rect()
        );

        for subsegment_tile_x in segment_tile_left..segment_tile_right {
            let (mut fill_from, mut fill_to) = (segment.from(), segment.to());
            let subsegment_tile_right =
                ((i32::from(subsegment_tile_x) + 1) * TILE_HEIGHT as i32) as f32;
            if subsegment_tile_right < segment_right {
                let x = subsegment_tile_right;
                let point = Vector2F::new(x, segment.solve_y_for_x(x));
                if !winding {
                    fill_to = point;
                    segment = LineSegment2F::new(point, segment.to());
                } else {
                    fill_from = point;
                    segment = LineSegment2F::new(segment.from(), point);
                }
            }

            let fill_segment = LineSegment2F::new(fill_from, fill_to);
            let fill_tile_coords = Vector2I::new(subsegment_tile_x, tile_y);
            self.add_fill(builder, fill_segment, fill_tile_coords);
        }
    }

    #[inline]
    pub(crate) fn tile_coords_to_local_index(&self, coords: Vector2I) -> Option<u32> {
        self.tiles.coords_to_index(coords).map(|index| index as u32)
    }

    #[inline]
    pub(crate) fn local_tile_index_to_coords(&self, tile_index: u32) -> Vector2I {
        self.tiles.index_to_coords(tile_index as usize)
    }
}
