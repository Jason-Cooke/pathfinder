// pathfinder/renderer/src/manager.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Directs the rendering of a scene and manages tile caching policies.

use crate::builder::{SceneBuilder, SceneTiles};
use crate::command::RenderCommand;
use crate::concurrent::executor::Executor;
use crate::scene::Scene;
use pathfinder_content::clip::PolygonClipper3D;
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::transform2d::Transform2DF;
use pathfinder_geometry::transform3d::Perspective;
use pathfinder_geometry::vector::{Vector2F, Vector4F};
use std::borrow::Cow;
use std::time::Duration;

#[derive(Clone)]
pub struct SceneManager {
    // FIXME(pcwalton): Should this be public? Changes to it might invalidate
    // cached data…
    pub scene: Scene,

    // Cache
    cached_data: Option<CachedData>,

    // Options
    cache_policy: CachePolicy,
    render_transform: RenderTransform,
    options: BuildOptions,
}

impl SceneManager {
    #[inline]
    pub fn new() -> SceneManager {
        SceneManager::from_scene(Scene::new())
    }

    pub fn from_scene(scene: Scene) -> SceneManager {
        SceneManager {
            scene,

            cached_data: None,

            cache_policy: CachePolicy::Never,
            render_transform: RenderTransform::Transform2D(Transform2DF::default()),
            options: BuildOptions::default(),
        }
    }

    #[inline]
    pub fn set_cache_policy(&mut self, new_cache_policy: CachePolicy) {
        self.cache_policy = new_cache_policy
    }

    #[inline]
    pub fn set_2d_transform(&mut self, new_transform: &Transform2DF) {
        self.render_transform = RenderTransform::Transform2D(*new_transform)
    }

    #[inline]
    pub fn set_perspective_transform(&mut self, new_perspective: &Perspective) {
        self.render_transform = RenderTransform::Perspective(*new_perspective)
    }

    #[inline]
    pub fn set_dilation(&mut self, new_dilation: Vector2F) {
        self.options.dilation = new_dilation
    }

    #[inline]
    pub fn set_subpixel_aa_enabled(&mut self, enabled: bool) {
        self.options.subpixel_aa_enabled = enabled
    }

    pub fn build<E>(&mut self, listener: Box<dyn RenderCommandListener>, executor: &E)
                    where E: Executor {
        // Build tiles if applicable.
        let (build_time, scene_tiles);
        if self.scene_is_dirty() {
            let prepared_render_transform = self.render_transform.prepare(self.scene.bounds());
            let (new_scene_tiles, new_build_time) =
                SceneBuilder::new(&self.scene,
                                  prepared_render_transform,
                                  &self.options,
                                  &*listener).build(executor);

            build_time = new_build_time;
            match self.render_transform {
                RenderTransform::Transform2D(transform) if
                        self.cache_policy == CachePolicy::OnTranslation => {
                    self.cached_data = Some(CachedData { transform, tiles: new_scene_tiles });
                    scene_tiles = Cow::Borrowed(&self.cached_data.as_ref().unwrap().tiles);
                }
                _ => scene_tiles = Cow::Owned(new_scene_tiles),
            }
        } else {
            build_time = Duration::default();
            scene_tiles = Cow::Borrowed(&self.cached_data.as_ref().unwrap().tiles);
        }

        // Send tile composite commands.
        // TODO(pcwalton): Send new matrix.
        scene_tiles.send(&listener);
        listener.send(RenderCommand::Finish { build_time });
    }

    fn scene_is_dirty(&self) -> bool {
        if self.cache_policy == CachePolicy::Never {
            return true;
        }
        let current_transform = match self.render_transform {
            RenderTransform::Perspective(_) => return true,
            RenderTransform::Transform2D(ref transform) => transform,
        };
        let cached_transform = match self.cached_data {
            None => return true,
            Some(ref cached_data) => &cached_data.transform,
        };
        cached_transform.matrix != current_transform.matrix
    }
}

/// How tiles are cached from frame to frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CachePolicy {
    /// No caching is performed.
    Never,
    /// The full scene is prerendered to tiles without regard for view box.
    /// Tiles are cached from frame to frame when the translation changes.
    /// If scale, skew, or rotation change, then we tile again.
    OnTranslation,
}

#[derive(Clone)]
struct CachedData {
    transform: Transform2DF,
    tiles: SceneTiles,
}

#[derive(Clone)]
enum RenderTransform {
    Transform2D(Transform2DF),
    Perspective(Perspective),
}

impl Default for RenderTransform {
    #[inline]
    fn default() -> RenderTransform {
        RenderTransform::Transform2D(Transform2DF::default())
    }
}

impl RenderTransform {
    fn prepare(&self, bounds: RectF) -> PreparedRenderTransform {
        let perspective = match self {
            RenderTransform::Transform2D(ref transform) => {
                if transform.is_identity() {
                    return PreparedRenderTransform::None;
                }
                return PreparedRenderTransform::Transform2D(*transform);
            }
            RenderTransform::Perspective(ref perspective) => *perspective,
        };

        let mut points = vec![
            bounds.origin().to_3d(),
            bounds.upper_right().to_3d(),
            bounds.lower_right().to_3d(),
            bounds.lower_left().to_3d(),
        ];
        debug!("-----");
        debug!("bounds={:?} ORIGINAL quad={:?}", bounds, points);
        for point in &mut points {
            *point = perspective.transform.transform_point(*point);
        }
        debug!("... PERSPECTIVE quad={:?}", points);

        // Compute depth.
        let quad = [
            points[0].perspective_divide(),
            points[1].perspective_divide(),
            points[2].perspective_divide(),
            points[3].perspective_divide(),
        ];
        debug!("... PERSPECTIVE-DIVIDED points = {:?}", quad);

        points = PolygonClipper3D::new(points).clip();
        debug!("... CLIPPED quad={:?}", points);
        for point in &mut points {
            *point = point.perspective_divide()
        }

        let inverse_transform = perspective.transform.inverse();
        let clip_polygon = points
            .into_iter()
            .map(|point| {
                inverse_transform
                    .transform_point(point)
                    .perspective_divide()
                    .to_2d()
            })
            .collect();
        return PreparedRenderTransform::Perspective {
            perspective,
            clip_polygon,
            quad,
        };
    }
}

pub trait RenderCommandListener: Send + Sync {
    fn send(&self, command: RenderCommand);
}

impl<F> RenderCommandListener for F
where
    F: Fn(RenderCommand) + Send + Sync,
{
    #[inline]
    fn send(&self, command: RenderCommand) {
        (*self)(command)
    }
}

#[derive(Copy, Clone, Default)]
pub(crate) struct BuildOptions {
    pub(crate) dilation: Vector2F,
    pub(crate) subpixel_aa_enabled: bool,
}

pub(crate) type BoundingQuad = [Vector4F; 4];

pub(crate) enum PreparedRenderTransform {
    None,
    Transform2D(Transform2DF),
    Perspective {
        perspective: Perspective,
        clip_polygon: Vec<Vector2F>,
        quad: [Vector4F; 4],
    },
}

impl PreparedRenderTransform {
    #[inline]
    pub(crate) fn bounding_quad(&self) -> BoundingQuad {
        match *self {
            PreparedRenderTransform::Perspective { quad, .. } => quad,
            _ => [Vector4F::default(); 4],
        }
    }

    #[inline]
    pub(crate) fn is_2d(&self) -> bool {
        match *self {
            PreparedRenderTransform::Transform2D(_) => true,
            _ => false,
        }
    }
}
