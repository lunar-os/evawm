use smithay::{
    backend::renderer::{
        element::{AsRenderElements, surface::WaylandSurfaceRenderElement},
        gles::GlesRenderer,
    },
    desktop::layer_map_for_output,
    utils::Scale,
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use super::OutputRenderElements;

pub(super) fn add_layer_elements(
    renderer: &mut GlesRenderer,
    output: &smithay::output::Output,
    layer: WlrLayer,
    scale: Scale<f64>,
    elements: &mut Vec<OutputRenderElements>,
) {
    let map = layer_map_for_output(output);
    for surface in map.layers_on(layer).rev() {
        let Some(geometry) = map.layer_geometry(surface) else {
            continue;
        };
        let rendered = surface.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            geometry.loc.to_f64().to_physical(scale).to_i32_round(),
            scale,
            1.0,
        );
        elements.extend(rendered.into_iter().map(OutputRenderElements::Layer));
    }
}
