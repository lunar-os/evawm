pub(crate) mod binds;
mod parse;
mod types;

pub use types::*;

pub const DEFAULT_CONFIG: &str = include_str!("../../config/config.kdl");

#[cfg(test)]
mod tests {
    use std::fs;

    use smithay::utils::{Point, Transform};

    use crate::{animation::Kind, workspace::Mode};

    use super::*;

    #[test]
    fn shipped_config_parses() {
        let config = Config::parse(DEFAULT_CONFIG).expect("default config parses");
        assert!(
            config
                .binds
                .iter()
                .any(|bind| matches!(bind.action, Action::Quit))
        );
        assert!(
            config
                .binds
                .iter()
                .any(|bind| matches!(bind.action, Action::CloseWindow))
        );
        assert_eq!(config.layout.gaps, 8.0);
        assert_eq!(config.layout.inner_border.width, 3.0);
        assert_eq!(config.layout.outer_border.width, 3.0);
        assert!(config.xwayland.enabled);
        assert_eq!(config.canvas.pan_step, 200.0);
        assert_eq!(config.canvas.pan_button, MouseButton::None);
        assert_eq!(config.canvas.mod_button, MouseButton::None);
        assert_eq!(config.cursor.size, 24);
        assert!(config.animations.slowdown > 0.0);
    }

    #[test]
    fn xwayland_is_enabled_by_default_and_can_be_disabled() {
        assert!(Config::default().xwayland.enabled);
        let config = Config::parse("xwayland { enabled false; }").expect("xwayland config parses");
        assert!(!config.xwayland.enabled);
    }

    #[test]
    fn new_surface_maps_to_runtime_config() {
        let config = Config::parse(
            r##"
            input {
                cursor { theme "plan9"; size 32; }
                keyboard { repeat-delay 400; repeat-rate 12; }
                mouse {
                    accel-profile "adaptive"
                    accel-speed 0.5
                    natural-scroll
                    pan-button "middle"
                    mod-button "back"
                }
                touchpad { tap false; natural-scroll false; }
                focus-follows-mouse { scroll false; canvas true; }
            }

            layout {
                gaps 15
                inner-border { width 1; active-color "#ffffff"; inactive-color "#505050"; }
                outer-border { width 6; active-color "#0f0f0f"; inactive-color "#0f0f0f"; }
                background-color "#222222"
                default-column-width { proportion 0.42; }
                canvas {
                    pan-step 300
                    move-step 55
                    new-window-width 0.5
                    new-window-height 0.7
                }
            }
            "##,
        )
        .expect("config parses");

        assert_eq!(config.cursor.theme, "plan9");
        assert_eq!(config.cursor.size, 32);
        assert_eq!(config.input.repeat_delay, 400);
        assert_eq!(config.input.repeat_rate, 12);
        assert_eq!(config.input.mouse.accel_speed, 0.5);
        assert_eq!(
            config.input.mouse.accel_profile,
            Some(AccelProfile::Adaptive)
        );
        assert!(config.input.mouse.natural_scroll);
        assert_eq!(config.canvas.pan_button, MouseButton::Middle);
        assert_eq!(config.canvas.mod_button, MouseButton::Back);
        assert!(!config.input.touchpad.tap);
        assert!(!config.input.touchpad.natural_scroll);
        assert!(!config.input.focus_follows_mouse.scroll);
        assert!(config.input.focus_follows_mouse.canvas);
        assert_eq!(config.layout.gaps, 15.0);
        assert_eq!(config.layout.inner_border.width, 1.0);
        assert_eq!(config.layout.outer_border.width, 6.0);
        assert_eq!(config.layout.default_column_proportion, 0.42);
        assert_eq!(config.canvas.pan_step, 300.0);
        assert_eq!(config.canvas.move_step, 55.0);
        assert_eq!(config.canvas.new_window_width, 0.5);
        assert_eq!(config.canvas.new_window_height, 0.7);
    }

    #[test]
    fn startup_section_accepts_spawn_and_spawn_sh() {
        let config = Config::parse(
            r##"
            startup {
                spawn "foot"
                spawn "sh" "-c" "echo hello"
                spawn-sh "exec foot"
            }
            "##,
        )
        .expect("startup parses");

        assert_eq!(
            config.startup,
            vec![
                vec!["foot".to_string()],
                vec!["sh".to_string(), "-c".to_string(), "echo hello".to_string()],
                vec!["sh".to_string(), "-c".to_string(), "exec foot".to_string()],
            ]
        );
    }

    #[test]
    fn binds_parse_actions_and_replace_the_set() {
        let config = Config::parse(
            r##"
            binds {
                Mod+Q { close-window; }
                Mod+Shift+E { quit; }
                Mod+Return { spawn "foot"; }
                Mod+1 { focus-workspace 1; }
                Mod+Minus { resize-width "-10%"; }
                Mod+Tab { cycle-window "next"; }
                Mod+Alt+Left { pan-camera "left"; }
            }
            "##,
        )
        .expect("binds parse");

        assert_eq!(config.binds.len(), 7);
        assert!(matches!(config.binds[0].action, Action::CloseWindow));
        assert!(matches!(config.binds[1].action, Action::Quit));
        assert!(matches!(&config.binds[2].action, Action::Spawn(args) if args[0] == "foot"));
        assert!(matches!(config.binds[3].action, Action::FocusWorkspace(1)));
        assert!(
            matches!(&config.binds[4].action, Action::ResizeWidth(value) if (*value + 0.10).abs() < 0.001)
        );
        assert!(matches!(config.binds[5].action, Action::CycleWindowNext));
        assert!(matches!(
            config.binds[6].action,
            Action::PanCameraLeft(None)
        ));
    }

    #[test]
    fn user_binds_replace_shipped_binds() {
        let mut config = Config::parse(DEFAULT_CONFIG).expect("default config parses");
        let default_binds = config.binds.len();
        config
            .apply("binds {\n    Mod+Q { close-window; }\n}")
            .expect("user binds parse");
        assert_eq!(config.binds.len(), 1);
        assert!(default_binds > 1);
    }

    #[test]
    fn output_blocks_merge_by_name() {
        let config = Config::parse(
            r##"
            output "DP-1" {
                mode "1920x1080@165"
                position x=0 y=1080
            }
            output "DP-1" {
                transform "180";
            }
            "##,
        )
        .expect("outputs parse");

        assert_eq!(config.outputs.len(), 1);
        let output = &config.outputs[0];
        assert_eq!(output.name, "DP-1");
        assert_eq!(output.mode.expect("mode").refresh, Some(165_000));
        assert_eq!(output.position, Some(Point::from((0, 1080))));
        assert_eq!(output.transform, Some(Transform::_180));
    }

    #[test]
    fn animations_merge_partial_sections() {
        let mut config = Config::parse(DEFAULT_CONFIG).expect("default config parses");
        config
            .apply(
                r##"
                animations {
                    slowdown 1.25
                    window-open {
                        spring damping-ratio=0.8 stiffness=700 epsilon=0.001
                        scale-from 0.8
                        opacity-from 0.2
                    }
                    window-close { scale-to 0.75; }
                }
                "##,
            )
            .expect("animations parse");

        assert_eq!(config.animations.slowdown, 1.25);
        assert_eq!(config.animations.window_open.scale_from, 0.8);
        assert_eq!(config.animations.window_open.opacity_from, 0.2);
        assert_eq!(config.animations.window_close_scale, 0.75);
        assert!(matches!(
            config.animations.window_open.animation.kind,
            Kind::Spring { .. }
        ));
    }

    #[test]
    fn unknown_section_warns_by_default() {
        let config = Config::parse(
            r##"
            unknown-section { value 1; }
            layout { gaps 11; }
            "##,
        )
        .expect("lenient unknown section keeps parsing");
        assert_eq!(config.layout.gaps, 11.0);
    }

    #[test]
    fn unknown_option_inside_a_section_warns_by_default() {
        let config = Config::parse(
            r##"
            layout {
                gaps 11;
                no-such-option 1;
            }
            "##,
        )
        .expect("unknown child option is lenient");
        assert_eq!(config.layout.gaps, 11.0);
    }

    #[test]
    fn strict_unknown_option_inside_a_section_is_an_error() {
        let result = Config::parse(
            r##"
            strict true
            layout {
                gaps 11;
                no-such-option 1;
            }
            "##,
        );
        assert!(result.is_err());
    }

    #[test]
    fn strict_unknown_section_is_an_error() {
        let result = Config::parse(
            r##"
            strict true
            unknown-section { value 1; }
            "##,
        );
        assert!(result.is_err());
    }

    #[test]
    fn failed_include_does_not_apply_partial_sections() {
        let dir = std::env::temp_dir().join(format!(
            "evawm-config-bad-include-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp config dir");
        let main_path = dir.join("config.kdl");
        let include_path = dir.join("bad.kdl");
        fs::write(&include_path, "layout { gaps 12;\n").expect("write bad include");
        fs::write(&main_path, "layout { gaps 20; }\ninclude \"bad.kdl\"\n").expect("write main");

        let (config, _) = Config::load(Some(&main_path)).expect("config loads");
        assert_eq!(config.layout.gaps, 20.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn canvas_new_window_position_parses_and_validates() {
        let center = Config::parse(r#"layout { canvas { new-window-position "center"; }; }"#)
            .expect("center parses");
        assert_eq!(center.canvas.new_window_position, NewWindowPosition::Center);

        let cursor_center =
            Config::parse(r#"layout { canvas { new-window-position "cursor-center"; }; }"#)
                .expect("cursor-center parses");
        assert_eq!(
            cursor_center.canvas.new_window_position,
            NewWindowPosition::CursorCenter
        );

        let cursor_alias = Config::parse(r#"layout { canvas { new-window-position "cursor"; }; }"#)
            .expect("cursor alias parses");
        assert_eq!(
            cursor_alias.canvas.new_window_position,
            NewWindowPosition::CursorCenter
        );

        let cursor_top_left =
            Config::parse(r#"layout { canvas { new-window-position "cursor-top-left"; }; }"#)
                .expect("cursor-top-left parses");
        assert_eq!(
            cursor_top_left.canvas.new_window_position,
            NewWindowPosition::CursorTopLeft
        );

        assert!(
            Config::parse(
                r#"
                strict true
                layout {
                    canvas {
                        new-window-position "top-left"
                    }
                }
                "#,
            )
            .is_err()
        );
    }

    #[test]
    fn xkb_config_parses_and_clears_options() {
        let config = Config::parse(
            r#"
            input {
                keyboard {
                    xkb {
                        rules "evdev"
                        model "pc105"
                        layout "us,gb"
                        variant ",colemak"
                        options "caps:escape,grp:alt_shift_toggle"
                    }
                }
            }
            "#,
        )
        .expect("xkb config parses");
        assert_eq!(config.input.xkb.rules, "evdev");
        assert_eq!(config.input.xkb.model, "pc105");
        assert_eq!(config.input.xkb.layout, "us,gb");
        assert_eq!(config.input.xkb.variant, ",colemak");
        assert_eq!(
            config.input.xkb.options.as_deref(),
            Some("caps:escape,grp:alt_shift_toggle")
        );

        let cleared = Config::parse(
            r#"
            input {
                keyboard {
                    xkb {
                        options "caps:escape"
                    }
                }
            }
            input {
                keyboard {
                    xkb {
                        options ""
                    }
                }
            }
            "#,
        )
        .expect("clearing options parses");
        assert_eq!(cleared.input.xkb.options, None);
    }

    #[test]
    fn urgent_color_is_rejected_in_strict_mode() {
        assert!(
            Config::parse(
                r##"
                strict true
                layout {
                    inner-border {
                        urgent-color "#e06c75"
                    }
                }
                "##,
            )
            .is_err()
        );
    }

    #[test]
    fn mouse_button_names_accept_back_forward_middle_none_and_reject_old_names() {
        let config = Config::parse(
            r#"
            input {
                mouse {
                    pan-button "back"
                    mod-button "forward"
                }
            }
            "#,
        )
        .expect("back/forward parse");
        assert_eq!(config.canvas.pan_button, MouseButton::Back);
        assert_eq!(config.canvas.mod_button, MouseButton::Forward);

        let disabled = Config::parse(
            r#"
            input {
                mouse {
                    pan-button "none"
                    mod-button "none"
                }
            }
            "#,
        )
        .expect("none parses");
        assert_eq!(disabled.canvas.pan_button, MouseButton::None);
        assert_eq!(disabled.canvas.mod_button, MouseButton::None);

        assert!(Config::parse(r#"strict true; input { mouse { pan-button "side"; }; }"#).is_err());
        assert!(Config::parse(r#"strict true; input { mouse { mod-button "extra"; }; }"#).is_err());
    }

    #[test]
    fn shipped_hjkl_binds_follow_standard_vim_directions() {
        use smithay::input::keyboard::Keysym;

        let config = Config::parse(DEFAULT_CONFIG).expect("default config parses");
        let find_action = |mods: Modifiers, sym: Keysym| {
            config
                .binds
                .iter()
                .find(|bind| bind.mods == mods && bind.trigger == BindTrigger::Key(sym))
                .map(|bind| bind.action.clone())
        };

        let mod_only = Modifiers {
            logo: true,
            ..Default::default()
        };
        assert!(matches!(
            find_action(mod_only, Keysym::h),
            Some(Action::FocusLeft)
        ));
        assert!(matches!(
            find_action(mod_only, Keysym::j),
            Some(Action::FocusDown)
        ));
        assert!(matches!(
            find_action(mod_only, Keysym::k),
            Some(Action::FocusUp)
        ));
        assert!(matches!(
            find_action(mod_only, Keysym::l),
            Some(Action::FocusRight)
        ));

        let mod_shift = Modifiers {
            logo: true,
            shift: true,
            ..Default::default()
        };
        assert!(matches!(
            find_action(mod_shift, Keysym::h),
            Some(Action::MoveWindowLeft)
        ));
        assert!(matches!(
            find_action(mod_shift, Keysym::j),
            Some(Action::MoveDown)
        ));
        assert!(matches!(
            find_action(mod_shift, Keysym::k),
            Some(Action::MoveUp)
        ));
        assert!(matches!(
            find_action(mod_shift, Keysym::l),
            Some(Action::MoveWindowRight)
        ));

        let mod_alt_shift = Modifiers {
            logo: true,
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert!(matches!(
            find_action(mod_alt_shift, Keysym::h),
            Some(Action::MoveLeft)
        ));
        assert!(matches!(
            find_action(mod_alt_shift, Keysym::l),
            Some(Action::MoveRight)
        ));
    }

    #[test]
    fn per_animation_off_flag_parses_and_overrides() {
        let config = Config::parse(
            r#"
            animations {
                view-movement { off; }
                window-open { off true; }
                window-close { off; }
                overview { off true; }
            }
            animations {
                window-open { off false; }
            }
            "#,
        )
        .expect("per-animation off parses");

        assert!(!config.animations.off);
        assert!(config.animations.view_movement.off);
        assert!(!config.animations.window_open.animation.off);
        assert!(config.animations.window_close.off);
        assert!(config.animations.overview.off);
        assert!(!config.animations.window_movement.off);
    }

    #[test]
    fn layout_default_mode_parses_and_validates() {
        let config = Config::parse(r#"layout { default-mode "canvas"; }"#).expect("parses");
        assert_eq!(config.layout.default_mode, Mode::Canvas);
        let config = Config::parse(r#"layout { default-mode "scroll"; }"#).expect("parses");
        assert_eq!(config.layout.default_mode, Mode::Scroll);
        assert!(Config::parse(r#"layout { default-mode "grid"; }"#).is_err());
    }

    #[test]
    fn include_strict_mode_does_not_leak_to_parent() {
        let dir = std::env::temp_dir().join(format!(
            "evawm-config-strict-include-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp config dir");
        let main = dir.join("config.kdl");
        let include = dir.join("strict.kdl");
        fs::write(&include, "strict true\nlayout { gaps 12; }\n").expect("write include");
        fs::write(
            &main,
            "include \"strict.kdl\"\nunknown-section { value 1; }\n",
        )
        .expect("write main");

        let (config, _) = Config::load(Some(&main)).expect("lenient parent still parses");
        assert_eq!(config.layout.gaps, 12.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn includes_merge_in_order_and_are_recorded() {
        let dir =
            std::env::temp_dir().join(format!("evawm-config-test-{}-{}", std::process::id(), 1));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp config dir");
        let main_path = dir.join("config.kdl");
        let include_path = dir.join("extra.kdl");
        fs::write(
            &include_path,
            "layout {\n    gaps 12;\n}\ninput {\n    cursor {\n        size 31;\n    }\n}",
        )
        .expect("write include");
        fs::write(&main_path, "include \"extra.kdl\"\nlayout { gaps 20; }").expect("write main");

        let (config, path) = Config::load(Some(&main_path)).expect("config loads");
        assert_eq!(path, Some(main_path));
        assert_eq!(config.layout.gaps, 20.0);
        assert_eq!(config.cursor.size, 31);
        assert!(config.sources.iter().any(|source| source == &include_path));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_include_is_lenient_without_strict() {
        let config = Config::parse(
            r##"
            include "definitely-missing-evawm-config.kdl"
            layout { gaps 9; }
            "##,
        )
        .expect("missing include is lenient");
        assert_eq!(config.layout.gaps, 9.0);
    }

    #[test]
    fn missing_include_is_fatal_with_strict() {
        let result = Config::parse(
            r##"
            strict true
            include "definitely-missing-evawm-config.kdl"
            "##,
        );
        assert!(result.is_err());
    }

    #[test]
    fn invalid_actions_are_skipped_without_strict() {
        let config = Config::parse(
            r##"
            binds {
                Mod+Q { close-window; }
                Mod+X { no-such-action; }
            }
            "##,
        )
        .expect("invalid action is skipped");
        assert_eq!(config.binds.len(), 1);
    }

    #[test]
    fn invalid_actions_fail_with_strict() {
        let result = Config::parse(
            r##"
            strict true
            binds {
                Mod+X { no-such-action; }
            }
            "##,
        );
        assert!(result.is_err());
    }
    #[test]
    fn non_ascii_colors_are_rejected_without_panicking() {
        for color in ["#aéabc", "#aébcdef"] {
            let source = format!("layout {{ inner-border {{ active-color \"{color}\"; }} }}");
            assert!(
                Config::parse(&source).is_err(),
                "{color} should be rejected"
            );
        }
    }

    #[test]
    fn startup_preserves_interleaved_spawn_and_spawn_sh_order() {
        let config = Config::parse(
            r##"
            startup {
                spawn "first" "a"
                spawn-sh "second"
                spawn "third"
                spawn-sh "fourth"
            }
            "##,
        )
        .expect("startup parses");
        assert_eq!(
            config.startup,
            vec![
                vec!["first", "a"],
                vec!["sh", "-c", "second"],
                vec!["third"],
                vec!["sh", "-c", "fourth"],
            ]
        );
    }

    #[test]
    fn per_file_strict_does_not_leak_across_includes_in_either_direction() {
        let dir =
            std::env::temp_dir().join(format!("evawm-strict-per-file-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");

        // Lenient parent includes strict child that fails: parent stays lenient
        // and rolls back the failed include.
        let strict_bad = dir.join("strict_bad.kdl");
        fs::write(&strict_bad, "strict true\nunknown-child-key 1\n").unwrap();
        let lenient_parent = dir.join("lenient_parent.kdl");
        fs::write(
            &lenient_parent,
            "layout { gaps 12; }\ninclude \"strict_bad.kdl\"\n",
        )
        .unwrap();
        let (config, _) = Config::load(Some(&lenient_parent))
            .expect("lenient parent should survive a failing strict include");
        assert_eq!(config.layout.gaps, 12.0);

        // Strict parent includes child without `strict`: child starts lenient
        // (ignores unknown key) rather than inheriting parent's strictness.
        let lenient_child = dir.join("lenient_child.kdl");
        fs::write(&lenient_child, "layout { gaps 18; }\nunknown-option 1\n").unwrap();
        let strict_parent = dir.join("strict_parent.kdl");
        fs::write(
            &strict_parent,
            "strict true\ninclude \"lenient_child.kdl\"\n",
        )
        .unwrap();
        let (config, _) = Config::load(Some(&strict_parent))
            .expect("child without strict should default to lenient");
        assert_eq!(config.layout.gaps, 18.0);

        // Strict parent includes child with `strict false` that has a syntax
        // error: parent's strictness still rejects the failed include.
        let broken_child = dir.join("broken_child.kdl");
        fs::write(
            &broken_child,
            "strict false\nlayout { gaps \"not-a-number\"; }\n",
        )
        .unwrap();
        let strict_parent_fail = dir.join("strict_parent_fail.kdl");
        fs::write(
            &strict_parent_fail,
            "strict true\ninclude \"broken_child.kdl\"\n",
        )
        .unwrap();
        assert!(Config::load(Some(&strict_parent_fail)).is_err());

        // Lenient missing include is not added to `config.sources`, so
        // ConfigWatcher does not enter an infinite reload loop.
        let missing_parent = dir.join("missing_parent.kdl");
        fs::write(&missing_parent, "include \"does-not-exist.kdl\"\n").unwrap();
        let (config, _) = Config::load(Some(&missing_parent)).expect("lenient missing include");
        assert_eq!(config.sources.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overview_config_parses_and_clamps_zoom() {
        let config = Config::parse(
            r#"
            layout {
                overview {
                    zoom 0.45
                }
            }
            animations {
                overview {
                    duration-ms 180
                    curve "ease-out-cubic"
                }
            }
            binds {
                Mod+O { toggle-overview; }
                Mod+WheelScrollDown { open-overview; }
                Mod+WheelScrollUp { close-overview; }
            }
            "#,
        )
        .expect("overview config parses");
        assert!((config.layout.overview.zoom - 0.45).abs() < 1e-9);
        assert!(matches!(
            config.animations.overview.kind,
            Kind::Easing {
                duration_ms: 180,
                ..
            }
        ));
        assert!(
            config
                .binds
                .iter()
                .any(|bind| matches!(bind.action, Action::ToggleOverview))
        );
        let mod_only = Modifiers {
            logo: true,
            ..Modifiers::default()
        };
        assert!(matches!(
            config.find_wheel_bind(mod_only, WheelDirection::Down),
            Some(Action::OpenOverview)
        ));
        assert!(matches!(
            config.find_wheel_bind(mod_only, WheelDirection::Up),
            Some(Action::CloseOverview)
        ));

        let clamped_low = Config::parse("layout { overview { zoom 0.01; }; }").expect("parses");
        assert!((clamped_low.layout.overview.zoom - 0.1).abs() < 1e-9);
        let clamped_high = Config::parse("layout { overview { zoom 2.0; }; }").expect("parses");
        assert!((clamped_high.layout.overview.zoom - 0.95).abs() < 1e-9);
    }

    #[test]
    fn later_bind_overrides_earlier_duplicate() {
        let config = Config::parse(
            r#"
            binds {
                Mod+Q { close-window; }
                Mod+Q { toggle-overview; }
            }
            "#,
        )
        .expect("parses");
        let mods = Modifiers {
            logo: true,
            ..Modifiers::default()
        };
        let key = smithay::input::keyboard::Keysym::q;
        assert!(matches!(
            config.find_bind(mods, key, Some(key)),
            Some(Action::ToggleOverview)
        ));
    }

    #[test]
    fn partial_spring_override_preserves_existing_fields() {
        let mut config = Config::parse(DEFAULT_CONFIG).expect("default config parses");
        config
            .apply(
                r#"
                animations {
                    window-open {
                        spring damping-ratio=0.6
                    }
                }
                "#,
            )
            .expect("partial spring parses");
        match config.animations.window_open.animation.kind {
            Kind::Spring {
                damping_ratio,
                stiffness,
                epsilon,
            } => {
                assert!((damping_ratio - 0.6).abs() < 1e-9);
                assert!((stiffness - 800.0).abs() < 1e-9);
                assert!((epsilon - 0.001).abs() < 1e-9);
            }
            Kind::Easing { .. } => panic!("expected spring"),
        }
    }

    #[test]
    fn strict_rejects_extra_args_and_invalid_values() {
        assert!(Config::parse("strict true false").is_err());
        assert!(Config::parse("strict true\ninclude \"a.kdl\" \"b.kdl\"").is_err());
        assert!(
            Config::parse("strict true\nbinds { Mod+Q { close-window \"extra\"; }; }").is_err()
        );
        assert!(Config::parse("strict true\nbinds { Mod+Return { spawn; }; }").is_err());
        assert!(Config::parse("strict true\nbinds { Mod+Return { spawn \"cmd\" 1; }; }").is_err());
        assert!(
            Config::parse("strict true\nbinds { Mod+Left { pan-camera \"left\" \"bad\"; }; }")
                .is_err()
        );
        assert!(Config::parse("layout { background-color \"#+1+2+3\"; }").is_err());
        assert!(
            Config::parse(
                "animations { window-close { curve \"cubic-bezier\" 1.5 0.0 0.5 1.0; }; }"
            )
            .is_err()
        );
        assert!(Config::parse("output \"DP-1\" { position x=100; }").is_err());
    }
}
