// pathfinder/renderer/src/scene.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A set of paths to be rendered.

use crate::manager::{BuildOptions, PreparedRenderTransform};
use crate::paint::{Paint, PaintId};
use hashbrown::HashMap;
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::transform2d::Transform2DF;
use pathfinder_geometry::vector::Vector2F;
use pathfinder_content::color::ColorU;
use pathfinder_content::outline::Outline;

#[derive(Clone)]
pub struct Scene {
    pub(crate) paths: Vec<PathObject>,
    pub(crate) paints: Vec<Paint>,
    paint_cache: HashMap<Paint, PaintId>,
    bounds: RectF,
    view_box: RectF,
}

impl Scene {
    #[inline]
    pub fn new() -> Scene {
        Scene {
            paths: vec![],
            paints: vec![],
            paint_cache: HashMap::new(),
            bounds: RectF::default(),
            view_box: RectF::default(),
        }
    }

    pub fn push_path(&mut self, path: PathObject) {
        self.bounds = self.bounds.union_rect(path.outline.bounds());
        self.paths.push(path);
    }

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn push_paint(&mut self, paint: &Paint) -> PaintId {
        if let Some(paint_id) = self.paint_cache.get(paint) {
            return *paint_id;
        }

        let paint_id = PaintId(self.paints.len() as u16);
        self.paint_cache.insert(*paint, paint_id);
        self.paints.push(*paint);
        paint_id
    }

    #[inline]
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    #[inline]
    pub fn bounds(&self) -> RectF {
        self.bounds
    }

    #[inline]
    pub fn set_bounds(&mut self, new_bounds: RectF) {
        self.bounds = new_bounds;
    }

    #[inline]
    pub fn view_box(&self) -> RectF {
        self.view_box
    }

    #[inline]
    pub fn set_view_box(&mut self, new_view_box: RectF) {
        self.view_box = new_view_box;
    }

    pub(crate) fn apply_render_options(
        &self,
        original_outline: &Outline,
        transform: &PreparedRenderTransform,
        options: &BuildOptions,
    ) -> Outline {
        let effective_view_box = self.effective_view_box(options);

        let mut outline;
        match *transform {
            PreparedRenderTransform::Perspective {
                ref perspective,
                ref clip_polygon,
                ..
            } => {
                if original_outline.is_outside_polygon(clip_polygon) {
                    outline = Outline::new();
                } else {
                    outline = (*original_outline).clone();
                    outline.clip_against_polygon(clip_polygon);
                    outline.apply_perspective(perspective);

                    // TODO(pcwalton): Support subpixel AA in 3D.
                }
            }
            _ => {
                // TODO(pcwalton): Short circuit.
                outline = (*original_outline).clone();
                if transform.is_2d() || options.subpixel_aa_enabled {
                    let mut transform = match *transform {
                        PreparedRenderTransform::Transform2D(transform) => transform,
                        PreparedRenderTransform::None => Transform2DF::default(),
                        PreparedRenderTransform::Perspective { .. } => unreachable!(),
                    };
                    if options.subpixel_aa_enabled {
                        transform = transform
                            .post_mul(&Transform2DF::from_scale(Vector2F::new(3.0, 1.0)))
                    }
                    outline.transform(&transform);
                }
                outline.clip_against_rect(effective_view_box);
            }
        }

        if !options.dilation.is_zero() {
            outline.dilate(options.dilation);
        }

        // TODO(pcwalton): Fold this into previous passes to avoid unnecessary clones during
        // monotonic conversion.
        outline.prepare_for_tiling(self.effective_view_box(options));
        outline
    }

    pub fn monochrome_color(&self) -> Option<ColorU> {
        if self.paths.is_empty() {
            return None;
        }

        let first_paint_id = self.paths[0].paint;
        if self
            .paths
            .iter()
            .skip(1)
            .any(|path_object| path_object.paint != first_paint_id) {
            return None;
        }
        Some(self.paints[first_paint_id.0 as usize].color)
    }

    #[inline]
    pub(crate) fn effective_view_box(&self, options: &BuildOptions) -> RectF {
        if options.subpixel_aa_enabled {
            self.view_box.scale_xy(Vector2F::new(3.0, 1.0))
        } else {
            self.view_box
        }
    }
    
    pub fn paths<'a>(&'a self) -> PathIter {
        PathIter {
            scene: self,
            pos: 0
        }
    }
}

pub struct PathIter<'a> {
    scene: &'a Scene,
    pos: usize
}

impl<'a> Iterator for PathIter<'a> {
    type Item = (&'a Paint, &'a Outline, &'a str);
    fn next(&mut self) -> Option<Self::Item> {
        let item = self.scene.paths.get(self.pos).map(|path_object| {
            (
                self.scene.paints.get(path_object.paint.0 as usize).unwrap(),
                &path_object.outline,
                &*path_object.name
            )
        });
        self.pos += 1;
        item
    }
}

#[derive(Clone, Debug)]
pub struct PathObject {
    outline: Outline,
    paint: PaintId,
    name: String,
}

impl PathObject {
    #[inline]
    pub fn new(outline: Outline, paint: PaintId, name: String) -> PathObject {
        PathObject { outline, paint, name }
    }

    #[inline]
    pub fn outline(&self) -> &Outline {
        &self.outline
    }

    #[inline]
    pub(crate) fn paint(&self) -> PaintId {
        self.paint
    }
}
