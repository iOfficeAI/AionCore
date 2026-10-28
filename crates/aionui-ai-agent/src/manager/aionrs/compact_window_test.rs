use super::*;

#[test]
fn context_window_prefers_the_provider_limit() {
    assert_eq!(resolve_context_window(Some(100_000)), 100_000);
    // Garbage limits fall through to the environment / default instead of
    // producing a zero-token window that would compact on every turn.
    assert!(resolve_context_window(Some(0)) > 0);
    assert!(resolve_context_window(Some(-5)) > 0);
}

#[test]
fn engine_compact_window_keeps_aionrs_defaults_for_a_200k_window() {
    let mut compact = aion_config::compact::CompactConfig::default();
    apply_engine_compact_window(&mut compact, Some(200_000));

    assert_eq!(compact.context_window, 200_000);
    assert_eq!(compact.output_reserve, 20_000);
    assert_eq!(compact.autocompact_buffer, 13_000);
    assert_eq!(compact.emergency_buffer, 3_000);
    // 20K completion + 5% headroom keeps the summarization call inside 200K.
    assert_eq!(compact.autocompact_threshold_pct, Some(85));
}

#[test]
fn engine_compact_window_scales_to_a_smaller_model() {
    let mut compact = aion_config::compact::CompactConfig::default();
    apply_engine_compact_window(&mut compact, Some(100_000));

    assert_eq!(compact.context_window, 100_000);
    assert_eq!(compact.output_reserve, 10_000);
    assert_eq!(compact.autocompact_buffer, 6_500);
    assert_eq!(compact.emergency_buffer, 1_500);

    // The engine must react well before the provider's limit; the aionrs
    // default of 200K would have waited for ~167K here.
    let trigger = compact.context_window - compact.output_reserve - compact.autocompact_buffer;
    assert_eq!(trigger, 83_500);
    assert!(trigger < 100_000);
}

#[test]
fn engine_compact_trigger_leaves_room_for_the_fixed_20k_summary_completion() {
    // Regression: autocompact re-sends the whole conversation with a fixed 20K
    // completion request. A trigger derived only from output_reserve +
    // autocompact_buffer fired too late on a 131,077-token model and the
    // provider answered `Requested token count exceeds the model's maximum
    // context length` ("Autocompact failed").
    for window in [32_000i64, 65_536, 100_000, 131_072, 131_077, 200_000, 1_000_000] {
        let mut compact = aion_config::compact::CompactConfig::default();
        apply_engine_compact_window(&mut compact, Some(window));

        let pct = compact.autocompact_threshold_pct.expect("threshold must be pinned") as usize;
        let trigger = compact.context_window * pct / 100;
        assert!(
            trigger + 20_000 <= compact.context_window,
            "window {window}: trigger {trigger} + 20K completion overflows"
        );
        assert!(trigger > 0, "window {window}: no trigger band");
    }
}

#[test]
fn engine_compact_window_keeps_a_positive_trigger_band_for_tiny_windows() {
    for window in [4_000i64, 8_000, 16_000, 32_000] {
        let mut compact = aion_config::compact::CompactConfig::default();
        apply_engine_compact_window(&mut compact, Some(window));

        let trigger = compact
            .context_window
            .saturating_sub(compact.output_reserve)
            .saturating_sub(compact.autocompact_buffer);
        assert!(trigger > 0, "window {window} produced no trigger band");
        assert!(trigger < compact.context_window, "window {window} never triggers");
        assert!(compact.emergency_buffer < compact.context_window, "window {window}");
    }
}
