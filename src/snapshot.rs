//! Offscreen snapshots for whole-tile window transitions.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind as _, Color32F, ContextId, Frame as _, Offscreen as _, Renderer, Texture as _,
            damage::OutputDamageTracker,
            element::{
                Element, Id, Kind, RenderElement, UnderlyingStorage,
                utils::{Relocate, RelocateRenderElement},
            },
            gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture},
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    utils::{
        Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform, user_data::UserDataMap,
    },
};

/// Reusable offscreen texture for a single tile.
#[derive(Default)]
pub struct SnapshotBuffer {
    id: Option<Id>,
    commit: CommitCounter,
    texture: Option<GlesTexture>,
    texture_size: Size<i32, Buffer>,
    scale: Option<Scale<f64>>,
    context: Option<ContextId<GlesTexture>>,
    damage: Option<OutputDamageTracker>,
}

#[derive(Debug, Clone)]
pub struct SnapshotRenderElement {
    id: Id,
    texture: GlesTexture,
    context: ContextId<GlesTexture>,
    commit: CommitCounter,
    /// Screen-space rectangle in logical pixels.
    rect: Rectangle<f64, Logical>,
    alpha: f32,
}

impl SnapshotBuffer {
    /// Render `elements` into the offscreen texture and return a screen-space
    /// element for the result.
    pub fn render<E>(
        &mut self,
        renderer: &mut GlesRenderer,
        scale: Scale<f64>,
        elements: &[E],
    ) -> Option<SnapshotRenderElement>
    where
        E: RenderElement<GlesRenderer>,
    {
        let mut bounds: Option<Rectangle<i32, Physical>> = None;
        for element in elements {
            let geo = element.geometry(scale);
            if geo.is_empty() {
                continue;
            }
            bounds = Some(match bounds {
                Some(bounds) => bounds.merge(geo),
                None => geo,
            });
        }
        let bounds = bounds?;
        if bounds.size.w <= 0 || bounds.size.h <= 0 {
            return None;
        }

        let buffer_size = Size::<i32, Buffer>::from((bounds.size.w, bounds.size.h));
        let context = renderer.context_id();
        let recreated = self.texture.is_none()
            || self.texture_size != buffer_size
            || self.scale != Some(scale)
            || self.context.as_ref() != Some(&context);
        if recreated {
            let texture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
            self.texture = Some(texture);
            self.texture_size = buffer_size;
            self.scale = Some(scale);
            self.context = Some(context.clone());
            self.damage = Some(OutputDamageTracker::new(
                bounds.size,
                scale,
                Transform::Normal,
            ));
        }

        let texture = self.texture.as_mut()?;
        let damage = self.damage.as_mut()?;
        let relocated: Vec<_> = elements
            .iter()
            .map(|element| {
                RelocateRenderElement::from_element(
                    element,
                    bounds.loc.upscale(-1),
                    Relocate::Relative,
                )
            })
            .collect();
        let texture_element = texture.clone();
        let mut target = renderer.bind(texture).ok()?;
        let age = usize::from(!recreated);
        let result = damage
            .render_output(
                renderer,
                &mut target,
                age,
                &relocated,
                Color32F::TRANSPARENT,
            )
            .ok()?;
        if recreated || result.damage.is_some() {
            self.commit.increment();
        }

        let id = self.id.get_or_insert_with(Id::new).clone();
        let rect = Rectangle::new(
            bounds.loc.to_f64().to_logical(scale),
            bounds.size.to_f64().to_logical(scale),
        );
        Some(SnapshotRenderElement {
            id,
            texture: texture_element,
            context,
            commit: self.commit,
            rect,
            alpha: 1.0,
        })
    }
}

impl SnapshotRenderElement {
    pub fn rect(&self) -> Rectangle<f64, Logical> {
        self.rect
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha.clamp(0.0, 1.0);
        self
    }

    pub fn with_rect(mut self, rect: Rectangle<f64, Logical>) -> Self {
        self.rect = rect;
        self
    }

    pub fn center_physical(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        (self.rect.loc + Point::from((self.rect.size.w / 2.0, self.rect.size.h / 2.0)))
            .to_physical(scale)
            .to_i32_round()
    }
}

impl Element for SnapshotRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.rect.to_physical_precise_round(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size()).to_f64()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
        }
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for SnapshotRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        if frame.context_id() != self.context {
            return Ok(());
        }
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha,
            None,
            &[],
        )
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
