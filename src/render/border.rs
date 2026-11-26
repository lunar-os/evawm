use smithay::{
    backend::renderer::element::{Kind, solid::SolidColorRenderElement},
    desktop::Window,
    utils::{Logical, Point, Rectangle, Scale, Size},
};

use crate::{borders::BorderStyle, workspace::Workspace};

use super::WindowInnerRenderElements;

pub(super) fn border_style(workspace: &Workspace, focused: bool) -> Option<BorderStyle> {
    let layout = &workspace.scroll;
    let outer_width = layout.outer_border.width.max(0.0);
    let inner_width = layout.inner_border.width.max(0.0);
    if outer_width + inner_width <= 0.0 {
        return None;
    }

    let (outer_color, inner_color) = if focused {
        (layout.outer_border.active, layout.inner_border.active)
    } else {
        (layout.outer_border.inactive, layout.inner_border.inactive)
    };

    Some(BorderStyle {
        outer_width,
        inner_width,
        outer_color,
        inner_color,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn add_window_borders(
    camera_location: Point<f64, Logical>,
    window_states: &mut crate::window_state::WindowStates,
    window: &Window,
    border: Option<(Rectangle<f64, Logical>, BorderStyle)>,
    resize_tile_size: Option<Size<f64, Logical>>,
    alpha: f32,
    scale: Scale<f64>,
    elements: &mut Vec<WindowInnerRenderElements>,
) {
    if alpha <= 0.0 {
        return;
    }
    let Some((rect, style)) = border else {
        return;
    };
    // Do not flash a full-size frame before the client has committed its
    // first buffer; the open animation begins on that commit.
    let committed = window.geometry().size;
    if committed.w <= 0 || committed.h <= 0 {
        return;
    }

    // During a resize the whole frame follows the interpolated content size,
    // keeping the border thickness constant.
    let tile_size = resize_tile_size.unwrap_or(rect.size);
    let buffers = window_states.border_buffers_mut(window);
    buffers.update(
        tile_size,
        style.outer_width,
        style.inner_width,
        style.outer_color,
        style.inner_color,
    );

    let origin = (rect.loc - camera_location)
        .to_physical(scale)
        .to_i32_round();

    let mut push_ring = |ring: &crate::borders::BorderRing| {
        for strip in ring.strips() {
            if !strip.is_visible() {
                continue;
            }
            let offset = strip.rect.loc.to_f64().to_physical(scale).to_i32_round();
            elements.push(WindowInnerRenderElements::Border(
                SolidColorRenderElement::from_buffer(
                    &strip.buffer,
                    origin + offset,
                    scale,
                    alpha,
                    Kind::Unspecified,
                ),
            ));
        }
    };

    push_ring(&buffers.inner);
    push_ring(&buffers.outer);
}
