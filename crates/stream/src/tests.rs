use super::*;

fn cfg(w: u32, h: u32) -> StreamConfig {
    StreamConfig { width: w, height: h, fps: 30.0 }
}

fn chain(json: &str, c: StreamConfig) -> Result<Chain, StreamError> {
    let spec: ChainSpec = serde_json::from_str(json).map_err(|e| StreamError::BadSize(e.to_string()))?;
    Chain::new(&spec, &c)
}

fn frames(px: [u8; 4], n: usize, w: u32, h: u32) -> Vec<u8> {
    px.iter().copied().cycle().take(w as usize * h as usize * 4 * n).collect()
}

#[test]
fn a_chain_naming_what_doesnt_exist_is_refused_before_any_frame() {
    assert!(matches!(chain(r#"[{"id":"ec.nope"}]"#, cfg(4, 4)), Err(StreamError::UnknownEffect(_))));
    assert!(matches!(chain(r#"[{"id":"ec.channel.invert","params":{"nope":1}}]"#, cfg(4, 4)), Err(StreamError::UnknownParam { .. })));
    assert!(matches!(chain(r#"[{"id":"ec.blur.gaussian","params":{"blurriness":"lots"}}]"#, cfg(4, 4)), Err(StreamError::BadValue { .. })));
    assert!(matches!(chain("[]", cfg(0, 4)), Err(StreamError::BadSize(_))));
}

#[test]
fn every_whole_frame_comes_out_processed_and_a_partial_one_is_dropped() {
    let (w, h) = (8, 4);
    let mut c = chain(r#"{"effects":[{"id":"ec.channel.invert"}]}"#, cfg(w, h)).unwrap_or_else(|e| panic!("{e}"));
    let mut input = frames([250, 200, 10, 255], 2, w, h);
    input.extend_from_slice(&[1, 2, 3]);
    let mut out = Vec::new();
    let stats = run(&mut c, &mut input.as_slice(), &mut out, None, |_| {}).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(stats.frames, 2);
    assert_eq!(out.len(), w as usize * h as usize * 4 * 2);
    assert!(out[0] < 50 && out[2] > 200, "inverted: {:?}", &out[..4]);
}

#[test]
fn a_control_message_changes_the_next_frame_and_a_bad_one_is_reported_not_fatal() {
    let (w, h) = (4, 4);
    let mut c = chain(r#"[{"id":"ec.channel.invert"}]"#, cfg(w, h)).unwrap_or_else(|e| panic!("{e}"));
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = tx.send(Control::Bypass { effect: EffectRef::Index(0), on: true });
    let _ = tx.send(Control::Set { effect: EffectRef::Id("ec.nope".into()), param: "x".into(), value: serde_json::json!(1) });
    let input = frames([250, 200, 10, 255], 1, w, h);
    let (mut out, mut errors) = (Vec::new(), Vec::new());
    let stats = run(&mut c, &mut input.as_slice(), &mut out, Some(&rx), |e| errors.push(e)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((stats.frames, stats.controls, errors.len()), (1, 2, 1));
    assert_eq!(&out[..4], &[250, 200, 10, 255], "bypassed: the frame passes unchanged");
}

#[test]
fn an_effect_that_grows_the_layer_still_gives_frames_of_the_stream_size() {
    let (w, h) = (16, 8);
    let mut c = chain(r#"[{"id":"ec.blur.gaussian","params":{"blurriness":6}}]"#, cfg(w, h)).unwrap_or_else(|e| panic!("{e}"));
    let input = frames([255, 255, 255, 255], 1, w, h);
    let mut out = Vec::new();
    run(&mut c, &mut input.as_slice(), &mut out, None, |_| {}).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out.len(), w as usize * h as usize * 4);
}

#[test]
fn plain_json_values_take_the_parameters_kinds() {
    assert_eq!(coerce(&Value::Color([0.0; 4]), &serde_json::json!("#ff0080")).ok(), Some(Value::Color([1.0, 0.0, 128.0 / 255.0, 1.0])));
    assert_eq!(coerce(&Value::Vec2([0.0; 2]), &serde_json::json!([3, 4])).ok(), Some(Value::Vec2([3.0, 4.0])));
    assert_eq!(coerce(&Value::Enum(0), &serde_json::json!(2)).ok(), Some(Value::Enum(2)));
    assert!(coerce(&Value::Scalar(0.0), &serde_json::json!([1, 2])).is_err());
    assert!(parse_control("").is_none());
    assert!(matches!(parse_control(r#"{"reset": true}"#), Some(Ok(Control::Reset(true)))));
}

#[test]
fn the_list_offers_only_effects_that_need_nothing_but_the_frame() {
    let ids: Vec<&str> = chainable().iter().map(|s| s.id).collect();
    assert!(ids.contains(&"ec.blur.gaussian") && ids.contains(&"ec.channel.invert"));
    assert!(ids.iter().all(|id| !effectcraft_effects::is_time_dependent(id)));
}
