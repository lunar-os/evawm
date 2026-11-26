use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, surface::WaylandSurfaceRenderElement, utils::RescaleRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::Window,
    utils::{Point, Rectangle, Scale, Size},
};

use crate::{
    drag::Drag,
    state::EvaWM,
    workspace::{Mode, Workspace},
};

use super::{
    WindowInnerRenderElements,
    border::{add_window_borders, border_style},
};

/// Capture a window's current tile (content and borders) as one snapshot.
/// Used before removing a window so close animation never renders the border
/// and content as separate elements.
pub(crate) fn snapshot_tile(
    renderer: &mut GlesRenderer,
    state: &mut EvaWM,
    output_index: usize,
    window: &Window,
) -> Option<crate::snapshot::SnapshotRenderElement> {
    let (camera, scale, content_location, border_rect, style) = {
        let output_state = state.outputs.get(output_index)?;
        let scale = Scale::from(output_state.output.current_scale().fractional_scale());
        let camera = output_state.camera_location();
        let workspace = output_state
            .workspaces
            .iter()
            .find(|workspace| workspace.contains(window))?;
        if workspace.is_fullscreen(window) {
            tracing::debug!("snapshot tile: fullscreen");
            return None;
        }
        let focused = output_index == state.active_output
            && output_state
                .workspaces
                .get(output_state.active_workspace)
                .and_then(Workspace::focused_window)
                == Some(window);
        let style = border_style(workspace, focused);
        let inset = style.map_or(0.0, |style| style.outer_width + style.inner_width);
        let (content_location, border_rect) = if workspace.mode == Mode::Canvas {
            let rect = workspace.canvas.rect(window)?;
            (
                rect.loc,
                Rectangle::new(
                    rect.loc - Point::from((inset, inset)),
                    Size::from((rect.size.w + 2.0 * inset, rect.size.h + 2.0 * inset)),
                ),
            )
        } else {
            let rect = workspace.scroll.tile_rect(window)?;
            (Point::from((rect.loc.x + inset, rect.loc.y + inset)), rect)
        };
        (camera, scale, content_location, border_rect, style)
    };

    let geometry_loc = window.geometry().loc;
    let physical_location = (content_location - geometry_loc.to_f64() - camera)
        .to_physical(scale)
        .to_i32_round();
    let elements = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
        renderer,
        physical_location,
        scale,
        1.0,
    );
    let mut current: Vec<WindowInnerRenderElements> = elements
        .into_iter()
        .map(WindowInnerRenderElements::Layer)
        .collect();
    add_window_borders(
        camera,
        &mut state.window_states,
        window,
        style.map(|style| (border_rect, style)),
        None,
        1.0,
        scale,
        &mut current,
    );

    let mut snapshot = crate::snapshot::SnapshotBuffer::default();
    snapshot.render(renderer, scale, &current)
}

pub(super) fn elements(
    renderer: &mut GlesRenderer,
    state: &mut EvaWM,
    output_index: usize,
) -> (
    Vec<WindowInnerRenderElements>,
    Vec<WindowInnerRenderElements>,
) {
    let output_state = &state.outputs[output_index];
    let scale = Scale::from(output_state.output.current_scale().fractional_scale());
    let camera_location = output_state.camera_location();
    let now = state.animation_now();
    let active_workspace = output_state.active_workspace;
    let viewport = output_state.visible_world_viewport();
    let element_count = output_state.space.elements().len();
    let mut window_elements: Vec<WindowInnerRenderElements> = Vec::with_capacity(element_count * 9);
    let mut above_top_elements: Vec<WindowInnerRenderElements> =
        Vec::with_capacity(element_count.min(4));

    // Disjoint field borrows: the space iteration is immutable, while the
    // per-window border cache is updated in the same pass.
    let space = &output_state.space;
    let workspaces = &output_state.workspaces;
    let outputs = &state.outputs;
    #[allow(clippy::mutable_key_type)]
    let locations = &state.window_locations;
    let window_states = &mut state.window_states;
    let drag = &state.drag;
    let open_config = state.config.animations.window_open;
    // Reused across windows so element collection does not allocate per tile.
    let mut current: Vec<WindowInnerRenderElements> = Vec::new();

    // Closing snapshots animate above the remaining window stack (but below
    // Top/Overlay layer surfaces) so focus changes cannot cover or cut their
    // fade-out short. Pushing them first places them on top with zero copy.
    for element in window_states.closing_elements(
        output_index,
        workspaces[active_workspace].id,
        now,
        camera_location,
    ) {
        window_elements.push(WindowInnerRenderElements::Snapshot(element));
    }

    // Space elements are bottom-to-top; the output element list is top-to-
    // bottom, so walk in reverse like `space_render_elements` does.
    for window in space.elements().rev() {
        let Some(location) = space.element_location(window) else {
            continue;
        };

        let geometry_loc = window.geometry().loc;
        let content_location = location;
        // Indexed lookup avoids rescanning every workspace for every element
        // on every frame.
        let location_workspace = crate::state::workspace_location_in(outputs, locations, window);
        let workspace_index = location_workspace.and_then(|(location_output, workspace)| {
            (location_output == output_index).then_some(workspace)
        });
        let workspace = workspace_index.map(|index| &workspaces[index]);
        // During a cross-output drag the window is previewed in the target
        // output's Space while its layout membership is still in the source
        // output. Borrow the owning workspace for border and focus styling.
        let border_workspace = location_workspace.and_then(|(location_output, workspace)| {
            outputs.get(location_output)?.workspaces.get(workspace)
        });
        let layout = workspace.map(|workspace| &workspace.scroll);
        let workspace_fullscreen =
            workspace.is_some_and(|workspace| workspace.is_fullscreen(window));
        let output_size = workspace.map_or_else(
            || Size::from((1.0, 1.0)),
            |workspace| workspace.scroll.output_size,
        );
        let dragging = matches!(
            drag,
            Some(Drag::Move { window: dragged, .. })
                | Some(Drag::CanvasResize { window: dragged, .. })
                | Some(Drag::ColumnResize { window: dragged, .. })
                if dragged == window
        );
        // A dragged window renders live at its current space location. Ignore
        // any stale open/resize/layout transition so a quick re-grab cannot
        // flash a full-size or old-position frame.
        let transition = if dragging {
            None
        } else {
            window_states.transition_style(window, now)
        };
        let focused = location_workspace.is_some_and(|(location_output, ws)| {
            location_output == state.active_output
                && outputs
                    .get(location_output)
                    .and_then(|out| out.workspaces.get(ws))
                    .and_then(Workspace::focused_window)
                    == Some(window)
        });
        let style = if workspace_fullscreen && transition.is_none() {
            None
        } else {
            border_workspace.and_then(|workspace| border_style(workspace, focused))
        };
        let inset = style.map_or(0.0, |style| style.outer_width + style.inner_width);

        let forced_fullscreen = Rectangle::from_size(output_size);
        let override_rect = transition
            .map(|style| (style.rect, style.border_alpha))
            .or_else(|| workspace_fullscreen.then_some((forced_fullscreen, 0.0)));
        if override_rect.is_none() {
            let size = window.geometry().size.to_f64();
            let content = Rectangle::new(
                content_location.to_f64() - Point::from((inset, inset)),
                Size::from((size.w + 2.0 * inset, size.h + 2.0 * inset)),
            );
            if !content.overlaps(viewport) {
                continue;
            }
        }
        let transition_on_top = transition.map_or(workspace_fullscreen, |style| style.on_top);
        let transition_screen_space =
            transition.map_or(workspace_fullscreen, |style| style.screen_space);
        let resize = if override_rect.is_none() && !dragging {
            window_states.resize_visual(window, inset, now)
        } else {
            None
        };
        let open = if dragging {
            None
        } else {
            window_states.open_style(window, now, open_config)
        };

        let (physical_location, content_physical, transition_scale) =
            if let Some((rect, _)) = override_rect {
                let committed = window.geometry().size;
                let transition_scale = if committed.w > 0 && committed.h > 0 {
                    Scale::from((
                        rect.size.w / committed.w as f64,
                        rect.size.h / committed.h as f64,
                    ))
                } else {
                    Scale::from(1.0)
                };
                let content_location = if transition_screen_space {
                    rect.loc
                } else {
                    rect.loc - camera_location
                };
                let surface_location = content_location - geometry_loc.to_f64();
                (
                    surface_location.to_physical(scale).to_i32_round(),
                    content_location.to_physical(scale).to_i32_round(),
                    Some(transition_scale),
                )
            } else {
                let surface_location = (location - geometry_loc).to_f64() - camera_location;
                let content_location = location.to_f64() - camera_location;
                (
                    surface_location.to_physical(scale).to_i32_round(),
                    content_location.to_physical(scale).to_i32_round(),
                    None,
                )
            };

        let elements = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            physical_location,
            scale,
            1.0,
        );
        current.clear();

        if let Some(transition_scale) = transition_scale {
            for element in elements {
                current.push(WindowInnerRenderElements::ScaledWindow(
                    RescaleRenderElement::from_element(element, content_physical, transition_scale),
                ));
            }
        } else if let Some(resize) = &resize {
            for element in elements {
                current.push(WindowInnerRenderElements::ScaledWindow(
                    RescaleRenderElement::from_element(
                        element,
                        content_physical,
                        resize.content_scale,
                    ),
                ));
            }
        } else {
            current.extend(elements.into_iter().map(WindowInnerRenderElements::Layer));
        }

        let border = if let Some((content_rect, _)) = override_rect {
            style.map(|style| {
                let screen_rect = Rectangle::new(
                    content_rect.loc - Point::from((inset, inset)),
                    Size::from((
                        content_rect.size.w + 2.0 * inset,
                        content_rect.size.h + 2.0 * inset,
                    )),
                );
                let world_loc = if transition_screen_space {
                    screen_rect.loc + camera_location
                } else {
                    screen_rect.loc
                };
                (Rectangle::new(world_loc, screen_rect.size), style)
            })
        } else if dragging
            && let Some(style) = style
            && window.geometry().size.w > 0
            && window.geometry().size.h > 0
        {
            let committed = window.geometry().size.to_f64();
            Some((
                Rectangle::new(
                    content_location.to_f64() - Point::from((inset, inset)),
                    Size::from((committed.w + 2.0 * inset, committed.h + 2.0 * inset)),
                ),
                style,
            ))
        } else if let Some(workspace) = workspace
            && workspace.mode == Mode::Canvas
        {
            workspace
                .canvas
                .rects
                .get(window)
                .zip(style)
                .map(|(rect, style)| {
                    let border_rect = Rectangle::new(
                        rect.loc - Point::from((inset, inset)),
                        Size::from((rect.size.w + 2.0 * inset, rect.size.h + 2.0 * inset)),
                    );
                    (border_rect, style)
                })
        } else {
            layout.and_then(|layout| layout.tile_rect(window).zip(style))
        };

        let border_alpha = override_rect.map_or(1.0, |(_, border_alpha)| border_alpha);
        add_window_borders(
            camera_location,
            window_states,
            window,
            border,
            resize.map(|resize| resize.tile_size),
            border_alpha,
            scale,
            &mut current,
        );

        // Render the whole tile (content + border) to one texture and animate
        // that texture so the border cannot flash ahead of content on open.
        if let Some((open_alpha, open_scale)) = open
            && override_rect.is_none()
            && resize.is_none()
            && let Some(element) = window_states
                .open_snapshot_mut(window)
                .render(renderer, scale, &current)
        {
            let element = element.with_alpha(open_alpha);
            current.clear();
            current.push(if (open_scale - 1.0).abs() > 0.001 {
                let origin = element.center_physical(scale);
                WindowInnerRenderElements::ScaledSnapshot(RescaleRenderElement::from_element(
                    element, origin, open_scale,
                ))
            } else {
                WindowInnerRenderElements::Snapshot(element)
            });
        }

        // Fullscreen is a screen-space overlay above the Top layer. It stays
        // above its own output's bar even when keyboard focus moves to
        // another monitor. Other windows remain in the normal window stack.
        let target = if transition_on_top || workspace_fullscreen {
            &mut above_top_elements
        } else {
            &mut window_elements
        };
        target.append(&mut current);
    }

    (window_elements, above_top_elements)
}
