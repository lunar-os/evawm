use super::{
    Column, Layout, ResizeEdges,
    camera::fit_offset,
    constraints::clamp_client_size,
    geometry::{distribute_heights, resolve_width},
    resize_edges, resized_width,
};
use smithay::utils::{Logical, Point};

const VIEW: f64 = 1920.0;
const GAP: f64 = 8.0;

#[test]
fn borders_are_inside_the_tile() {
    let layout = Layout::new(
        smithay::utils::Size::from((1000, 800)),
        GAP,
        0.5,
        crate::config::BorderConfig {
            width: 2.0,
            ..Default::default()
        },
        crate::config::BorderConfig {
            width: 3.0,
            ..Default::default()
        },
    );
    assert_eq!(layout.frame_inset(), 5.0);
    assert_eq!(layout.full_height(), 784.0);
}

#[test]
fn config_change_updates_default_width_columns() {
    let mut layout = Layout::new(
        smithay::utils::Size::from((1000, 800)),
        GAP,
        0.5,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );
    layout.columns.push(Column {
        tiles: Vec::new(),
        active: 0,
        width: 100.0,
        x: 0.0,
        proportion: Some(0.5),
        saved: None,
        expanded: None,
    });
    layout.columns.push(Column {
        tiles: Vec::new(),
        active: 0,
        width: 100.0,
        x: 0.0,
        proportion: None,
        saved: None,
        expanded: None,
    });

    layout.set_options(
        10.0,
        0.7,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );

    assert_eq!(layout.gap, 10.0);
    assert_eq!(layout.default_proportion, 0.7);
    assert_eq!(layout.columns[0].proportion, Some(0.7));
    assert_eq!(layout.columns[1].proportion, None);
}

#[test]
fn moved_column_keeps_maximize_state_and_refits_to_target() {
    let mut layout = Layout::new(
        smithay::utils::Size::from((1000, 800)),
        GAP,
        0.5,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );
    layout.columns.push(Column {
        tiles: Vec::new(),
        active: 0,
        width: 300.0,
        x: 0.0,
        proportion: None,
        saved: Some((300.0, None)),
        expanded: None,
    });
    layout.active = 0;

    layout.ensure_active_column_maximized();
    assert_eq!(layout.columns[0].width, 1000.0 - 2.0 * GAP);
    assert_eq!(layout.columns[0].saved, Some((300.0, None)));

    layout.columns[0].width = 240.0;
    layout.columns[0].saved = None;
    layout.ensure_active_column_maximized();
    assert_eq!(layout.columns[0].width, 1000.0 - 2.0 * GAP);
    assert_eq!(layout.columns[0].saved, Some((240.0, None)));
}

#[test]
fn output_size_tracks_resizes_for_fullscreen() {
    let mut layout = Layout::new(
        smithay::utils::Size::from((1280, 800)),
        GAP,
        0.5,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );
    layout.set_output_size(smithay::utils::Size::from((1920, 1080)));
    assert_eq!(
        layout.output_size,
        smithay::utils::Size::from((1920.0, 1080.0))
    );
    assert_eq!(
        layout.working_area.size,
        smithay::utils::Size::from((1920.0, 1080.0))
    );
}

#[test]
fn client_size_constraints_are_clamped() {
    let size = smithay::utils::Size::from((900, 600));
    let min = smithay::utils::Size::from((300, 200));
    let max = smithay::utils::Size::from((800, 500));
    assert_eq!(
        clamp_client_size(size, min, max, false),
        smithay::utils::Size::from((800, 500))
    );
    assert_eq!(
        clamp_client_size(smithay::utils::Size::from((100, 100)), min, max, false),
        smithay::utils::Size::from((300, 200))
    );
    assert_eq!(
        clamp_client_size(size, min, max, true),
        smithay::utils::Size::from((900, 600))
    );
}

#[test]
fn tile_heights_split_evenly() {
    assert_eq!(
        distribute_heights(1000.0, GAP, &[None], &[1.0]),
        vec![1000.0]
    );
    assert_eq!(
        distribute_heights(1000.0, GAP, &[None, None], &[1.0, 1.0]),
        vec![496.0, 496.0]
    );
    assert_eq!(
        distribute_heights(1000.0, GAP, &[Some(400.0), None], &[1.0, 1.0]),
        vec![400.0, 592.0]
    );
    assert_eq!(
        distribute_heights(1000.0, GAP, &[None, None], &[700.0, 1.0]),
        vec![700.0, 292.0]
    );
}

#[test]
fn fixed_tile_heights_scale_to_fit() {
    let heights = distribute_heights(1000.0, GAP, &[Some(900.0), Some(900.0)], &[1.0, 1.0]);
    assert!(heights.iter().all(|height| *height >= 1.0));
    assert!((heights.iter().sum::<f64>() - (1000.0 - GAP)).abs() < 0.01);
}

#[test]
fn left_edge_resize_inverts_the_drag() {
    assert_eq!(resized_width(900.0, 100.0, false, 120.0, 1904.0), 1000.0);
    assert_eq!(resized_width(900.0, -100.0, true, 120.0, 1904.0), 1000.0);
    assert_eq!(resized_width(900.0, 2000.0, false, 120.0, 1904.0), 1904.0);
    assert_eq!(resized_width(900.0, -2000.0, false, 120.0, 1904.0), 120.0);

    let (x, old_camera, old_width) = (500.0, 100.0, 900.0);
    let new_width = resized_width(old_width, -100.0, true, 120.0, 1904.0);
    let new_camera = old_camera + (new_width - old_width);
    assert_eq!(x + old_width - old_camera, x + new_width - new_camera);
}

#[test]
fn resize_edges_are_thirds_like_niri() {
    let size = (900.0, 900.0);
    let point = |x: f64, y: f64| Point::<f64, Logical>::from((x, y));

    assert_eq!(
        resize_edges(point(100.0, 100.0), size.0, size.1),
        ResizeEdges {
            left: true,
            top: true,
            ..Default::default()
        }
    );
    assert_eq!(
        resize_edges(point(800.0, 100.0), size.0, size.1),
        ResizeEdges {
            right: true,
            top: true,
            ..Default::default()
        }
    );
    assert_eq!(
        resize_edges(point(800.0, 800.0), size.0, size.1),
        ResizeEdges {
            right: true,
            bottom: true,
            ..Default::default()
        }
    );
    assert_eq!(
        resize_edges(point(100.0, 800.0), size.0, size.1),
        ResizeEdges {
            left: true,
            bottom: true,
            ..Default::default()
        }
    );
    assert_eq!(
        resize_edges(point(450.0, 450.0), size.0, size.1),
        ResizeEdges::default()
    );
    assert_eq!(
        resize_edges(point(450.0, 100.0), size.0, size.1),
        ResizeEdges {
            top: true,
            ..Default::default()
        }
    );
}

#[test]
fn two_half_columns_fit_without_scrolling() {
    let width = resolve_width(VIEW, GAP, 0.5);
    assert_eq!(width, (VIEW - 3.0 * GAP) / 2.0);

    let second_x = GAP + width + GAP;
    assert_eq!(second_x + width + GAP, VIEW);
    assert_eq!(fit_offset(0.0, second_x, width, VIEW, GAP), 0.0);
}

#[test]
fn first_column_is_not_centered() {
    assert_eq!(fit_offset(0.0, GAP, 960.0, VIEW, GAP), 0.0);
}

#[test]
fn focusing_right_scrolls_just_enough() {
    let camera = fit_offset(0.0, 976.0, 960.0, VIEW, GAP);
    assert_eq!(camera, 24.0);
    let screen_right = 976.0 + 960.0 - camera;
    assert_eq!(screen_right, VIEW - GAP);
}

#[test]
fn focusing_left_returns_to_origin() {
    assert_eq!(fit_offset(24.0, GAP, 960.0, VIEW, GAP), 0.0);
}

#[test]
fn already_visible_column_does_not_move_the_view() {
    assert_eq!(fit_offset(0.0, 416.0, 400.0, VIEW, GAP), 0.0);
    assert_eq!(fit_offset(100.0, 416.0, 400.0, VIEW, GAP), 100.0);
}

#[test]
fn wide_column_is_left_aligned() {
    assert_eq!(fit_offset(500.0, GAP, 2000.0, VIEW, GAP), GAP);
}

#[test]
fn padding_shrinks_near_view_width() {
    assert_eq!(fit_offset(0.0, GAP, 1910.0, VIEW, GAP), GAP - 5.0);
}

fn test_layout_with_columns(widths: &[f64], active: usize) -> Layout {
    let mut layout = Layout::new(
        smithay::utils::Size::from((1000, 800)),
        GAP,
        0.5,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );
    let mut x = GAP;
    for &width in widths {
        layout.columns.push(Column {
            tiles: Vec::new(),
            active: 0,
            width,
            x,
            proportion: None,
            saved: None,
            expanded: None,
        });
        x += width + GAP;
    }
    layout.active = active;
    layout.camera_x = 0.0;
    layout
}

#[test]
fn expand_column_fills_space_after_visible_neighbours_and_restores() {
    let mut layout = test_layout_with_columns(&[300.0, 300.0, 300.0], 0);
    layout.expand_column_to_available_width();

    // 1000 - gap - (3 * (300 + gap)) = 68 extra pixels for the active column.
    assert_eq!(layout.columns[0].width, 368.0);
    assert_eq!(layout.columns[1].width, 300.0);
    assert_eq!(layout.columns[2].width, 300.0);
    assert!(layout.columns[0].expanded.is_some());

    layout.expand_column_to_available_width();
    assert_eq!(layout.columns[0].width, 300.0);
    assert!(layout.columns[0].expanded.is_none());
}

#[test]
fn expand_column_with_only_active_visible_toggles_like_maximize() {
    let mut layout = test_layout_with_columns(&[300.0, 2000.0], 0);
    layout.expand_column_to_available_width();

    assert_eq!(layout.columns[0].width, 1000.0 - 2.0 * GAP);
    assert!(layout.columns[0].saved.is_some());
    assert!(layout.columns[0].expanded.is_some());

    layout.expand_column_to_available_width();
    assert_eq!(layout.columns[0].width, 300.0);
    assert!(layout.columns[0].saved.is_none());
    assert!(layout.columns[0].expanded.is_none());
}

#[test]
fn expand_column_does_nothing_when_active_is_not_fully_visible() {
    let mut layout = test_layout_with_columns(&[300.0, 300.0], 1);
    layout.camera_x = 500.0;

    layout.expand_column_to_available_width();

    assert_eq!(layout.columns[1].width, 300.0);
    assert!(layout.columns[1].expanded.is_none());
}

#[test]
fn fixed_tiles_shrink_before_auto_tiles_overflow() {
    let heights = distribute_heights(300.0, GAP, &[Some(600.0), None], &[1.0, 1.0]);
    assert!(heights.iter().sum::<f64>() <= 300.0, "heights: {heights:?}");
    assert!(heights[0] > 1.0);
    assert!(heights[1] >= 1.0);
}

#[test]
fn initial_content_size_matches_proportion_and_constraints() {
    let layout = Layout::new(
        smithay::utils::Size::from((1920, 1080)),
        GAP,
        0.5,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );
    let inset = layout.frame_inset();
    let expected_w = (resolve_width(1920.0, GAP, 0.5) - 2.0 * inset).round() as i32;
    let expected_h = (layout.full_height() - 2.0 * inset).round() as i32;
    assert_eq!(
        layout.initial_content_size((0, 0).into(), (0, 0).into()),
        smithay::utils::Size::from((expected_w, expected_h))
    );
    // Fixed-size client keeps its exact requested dimensions.
    assert_eq!(
        layout.initial_content_size((640, 480).into(), (640, 480).into()),
        smithay::utils::Size::from((640, 480))
    );
}

#[test]
fn distribute_heights_reserves_auto_minimums_before_fixed_tiles() {
    let heights = distribute_heights(1000.0, GAP, &[Some(800.0), None], &[1.0, 300.0]);
    assert!(
        (heights.iter().sum::<f64>() - (1000.0 - GAP)).abs() < 0.01,
        "heights overflowed: {heights:?}"
    );
    assert!(
        (heights[1] - 300.0).abs() < 0.01,
        "auto min lost: {heights:?}"
    );
}

#[test]
fn expand_column_shifts_following_columns_and_preserves_scrolled_camera() {
    let mut layout = test_layout_with_columns(&[300.0, 300.0, 300.0], 1);
    // Scroll so column 1 sits at the left edge (camera_x = 308). Only columns
    // 1 and 2 are visible; 376 px of width remain to the right.
    layout.camera_x = 308.0;
    layout.expand_column_to_available_width();

    assert_eq!(layout.columns[1].width, 676.0);
    assert_eq!(layout.columns[2].x, layout.columns[1].x + 676.0 + GAP);
    assert_eq!(layout.camera_x, 308.0);
}

#[test]
fn set_output_size_resets_working_area_origin() {
    let mut layout = Layout::new(
        smithay::utils::Size::from((1280, 800)),
        GAP,
        0.5,
        crate::config::BorderConfig::default(),
        crate::config::BorderConfig::outer(),
    );
    layout.set_working_area(smithay::utils::Rectangle::new(
        (0, 32).into(),
        (1280, 768).into(),
    ));
    layout.set_output_size(smithay::utils::Size::from((1920, 1080)));
    assert_eq!(layout.working_area.loc, Point::from((0.0, 0.0)));
    assert_eq!(
        layout.working_area.size,
        smithay::utils::Size::from((1920.0, 1080.0))
    );
}
