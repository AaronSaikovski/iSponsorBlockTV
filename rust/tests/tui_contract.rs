use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use isponsorblocktv::{
    api::{ChannelResult, PairingResult},
    config::{ConfigDocument, SaveOutcome},
    setup::{
        state::{
            Action, Field, FoundDevice, Request, Screen, SetupEffect, SetupEvent, SetupState,
            Stamp, reduce,
        },
        tui::translate,
        widgets::{Edit, Surface, TextInput, render},
    },
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

fn state(value: Value) -> SetupState {
    SetupState::new(serde_json::from_value::<ConfigDocument>(value).unwrap())
}
fn act(state: &mut SetupState, action: Action) -> Vec<SetupEffect> {
    reduce(state, SetupEvent::Activate(action))
}
fn focus(state: &mut SetupState, field: Field) {
    state.focus = state
        .rows()
        .iter()
        .position(|row| row.field == Some(field))
        .unwrap();
}
fn input(state: &mut SetupState, field: Field, text: &str) -> Vec<SetupEffect> {
    focus(state, field);
    reduce(state, SetupEvent::Edit(Edit::SelectAll));
    reduce(state, SetupEvent::Edit(Edit::Insert(text.into())))
}
fn pair_stamp(effects: &[SetupEffect]) -> Stamp {
    effects
        .iter()
        .find_map(|effect| match effect {
            SetupEffect::Pair { stamp, .. } => Some(*stamp),
            _ => None,
        })
        .unwrap()
}
fn discovery_stamp(effects: &[SetupEffect]) -> Stamp {
    effects
        .iter()
        .find_map(|effect| match effect {
            SetupEffect::Discover { stamp } => Some(*stamp),
            _ => None,
        })
        .unwrap()
}
fn paired() -> PairingResult {
    PairingResult {
        screen_id: "screen".into(),
        name: Some("Living [room] 日本語".into()),
        lounge_token: Some("transient-secret".into()),
    }
}

#[test]
fn grapheme_motion_delete_selection_and_paste() {
    let mut input = TextInput::new("Aé👨‍👩‍👧‍👦e\u{301}界");
    input.edit(Edit::Left);
    input.edit(Edit::Backspace);
    assert_eq!(input.text, "Aé👨‍👩‍👧‍👦界");
    input.edit(Edit::Left);
    input.edit(Edit::Delete);
    assert_eq!(input.text, "Aé界");
    input.edit(Edit::Home);
    input.edit(Edit::Right);
    input.edit(Edit::Delete);
    assert_eq!(input.text, "A界");
    input.edit(Edit::SelectAll);
    input.edit(Edit::Insert("日本語\n\u{1b}[2J".into()));
    assert_eq!(input.text, "日本語[2J");
    input.edit(Edit::End);
    assert_eq!(input.cursor, input.text.len());
}
#[test]
fn input_scrolling_and_masking_use_terminal_cells() {
    let input = TextInput::new("abc界é");
    let (visible, cursor) = input.visible(false, 4);
    assert_eq!(visible, "界é");
    assert_eq!(cursor, 3);
    let (masked, cursor) = input.visible(true, 4);
    assert_eq!(masked, "***");
    assert_eq!(cursor, 3);
    let malicious = TextInput::new("name\u{1b}[2J\r\n");
    assert!(!malicious.visible(false, 80).0.chars().any(char::is_control));
    assert!(malicious.text.contains('\u{1b}'));
}
#[test]
fn main_exposes_every_setting_and_preserves_unknown_categories() {
    let mut state = state(
        json!({"join_name":"unchanged", "skip_categories":["sponsor","future"], "unknown":{"large":18446744073709551615_u64}}),
    );
    let actions: Vec<_> = state.rows().iter().map(|row| row.action.clone()).collect();
    for action in [
        Action::AddDevice,
        Action::AddChannel,
        Action::ShowKey,
        Action::Credentials,
        Action::Save,
        Action::Exit,
        Action::Input(Field::Minimum),
        Action::Input(Field::ApiKey),
        Action::Input(Field::BaseUrl),
        Action::Toggle("skip_count_tracking"),
        Action::Toggle("mute_ads"),
        Action::Toggle("skip_ads"),
        Action::Toggle("auto_play"),
        Action::Toggle("use_proxy"),
    ] {
        assert!(actions.contains(&action));
    }
    assert!(actions.contains(&Action::Category("future".into())));
    act(&mut state, Action::Category("hook".into()));
    assert_eq!(state.categories(), vec!["sponsor", "future", "hook"]);
    assert_eq!(state.draft.0["join_name"], "unchanged");
    assert_eq!(state.draft.0["unknown"], state.baseline.0["unknown"]);
}
#[test]
fn missing_null_and_empty_categories_remain_distinct_until_edited() {
    for value in [
        json!({}),
        json!({"skip_categories":null}),
        json!({"skip_categories":[]}),
    ] {
        let mut state = state(value);
        let before = state.draft.clone();
        assert!(!state.dirty());
        act(&mut state, Action::ShowKey);
        assert_eq!(state.draft, before);
    }
}

#[test]
fn focus_tracks_dynamic_rows_without_stale_category_or_result_indices() {
    fn assert_focus_order(state: &mut SetupState) {
        let (first, last) = match state.screen {
            Screen::Main => (Action::AddDevice, Action::Exit),
            Screen::AddDevice => (Action::DeviceTab(false), Action::Cancel),
            Screen::AddChannel => (Action::ChannelTab(false), Action::Cancel),
            _ => unreachable!(),
        };
        state.focus = usize::MAX;
        assert_eq!(state.focused_action(), None);
        assert_eq!(state.focused_field(), None);
        reduce(state, SetupEvent::Focus(usize::MAX));
        assert_eq!(state.focused_action(), Some(last.clone()));
        reduce(state, SetupEvent::Move(1));
        assert_eq!(state.focused_action(), Some(first));
        let end = translate(
            Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            state,
            &Surface::default(),
        )
        .unwrap();
        reduce(state, end);
        assert_eq!(state.focused_action(), Some(last));
    }

    let mut state = state(json!({
        "devices": [{"name": "界"}, {}, null],
        "channel_whitelist": [{"id": "UCfallback"}, {}, null],
        "skip_categories": ["future", "sponsor", "future", 7, null, "界", "intro"]
    }));
    let categories: Vec<_> = state
        .rows()
        .into_iter()
        .filter_map(|row| match row.action {
            Action::Category(category) => Some(category),
            _ => None,
        })
        .collect();
    let mut expected: Vec<_> = isponsorblocktv::setup::state::CATEGORIES
        .iter()
        .map(|category| (*category).to_owned())
        .collect();
    expected.extend(["future".into(), "界".into()]);
    assert_eq!(categories, expected);
    assert_focus_order(&mut state);
    act(&mut state, Action::RemoveDevice(0));
    act(&mut state, Action::RemoveChannel(1));
    act(&mut state, Action::Category("future".into()));
    assert_focus_order(&mut state);

    // Public state mutations must be visible immediately, not after a render.
    for categories in [json!(null), json!([]), json!(["new", "new", false])] {
        state.draft.0.insert("skip_categories".into(), categories);
        assert_focus_order(&mut state);
    }
    state.screen = Screen::AddDevice;
    state.discovered = vec![
        FoundDevice {
            screen_id: "one".into(),
            name: "界".into(),
            selected: true,
        },
        FoundDevice {
            screen_id: "two".into(),
            name: "Second".into(),
            selected: false,
        },
    ];
    for lan_tab in [true, false, true] {
        state.lan_tab = lan_tab;
        assert_focus_order(&mut state);
    }
    state.discovered.clear();
    assert_focus_order(&mut state);

    state.screen = Screen::AddChannel;
    state.channels = vec![ChannelResult {
        id: "UCresult".into(),
        name: "界".into(),
        subscribers: "Hidden".into(),
    }];
    state.selected_channel = Some(0);
    for manual_tab in [false, true, false] {
        state.manual_tab = manual_tab;
        assert_focus_order(&mut state);
    }
    state.channels.clear();
    assert_focus_order(&mut state);
}
#[test]
fn migration_requires_explicit_save_and_save_failure_keeps_draft() {
    let mut state = state(json!({"atvs":null,"unknown":7}));
    assert_eq!(state.screen, Screen::Migration);
    reduce(&mut state, SetupEvent::Escape);
    assert!(state.draft.0.contains_key("atvs"));
    let mut state = SetupState::new(state.draft);
    assert!(matches!(
        act(&mut state, Action::Migrate).as_slice(),
        [SetupEffect::Save {
            migration: true,
            exit: false
        }]
    ));
    assert!(!state.draft.0.contains_key("atvs"));
    reduce(
        &mut state,
        SetupEvent::Saved {
            result: Err("disk full".into()),
            exit: false,
            migration: true,
        },
    );
    assert!(state.baseline.0.contains_key("atvs"));
    assert_eq!(state.screen, Screen::Migration);
    reduce(
        &mut state,
        SetupEvent::Saved {
            result: Ok(SaveOutcome::Saved),
            exit: false,
            migration: true,
        },
    );
    assert_eq!(state.screen, Screen::Main);
    assert!(!state.dirty());
    assert_eq!(state.draft.0["unknown"], 7);
}
#[test]
fn dirty_exit_has_save_discard_cancel_and_real_outcomes() {
    let mut state = state(json!({}));
    act(&mut state, Action::Toggle("mute_ads"));
    act(&mut state, Action::Exit);
    assert_eq!(state.screen, Screen::ConfirmExit);
    act(&mut state, Action::Cancel);
    assert_eq!(state.screen, Screen::Main);
    assert!(state.dirty());
    act(&mut state, Action::Exit);
    assert!(matches!(
        act(&mut state, Action::SaveExit).as_slice(),
        [SetupEffect::Save {
            exit: true,
            migration: false
        }]
    ));
    reduce(
        &mut state,
        SetupEvent::Saved {
            result: Err("permission denied".into()),
            exit: true,
            migration: false,
        },
    );
    assert_eq!(state.screen, Screen::ConfirmExit);
    assert!(!state.finished);
    assert!(state.dirty());
    reduce(
        &mut state,
        SetupEvent::Saved {
            result: Ok(SaveOutcome::Saved),
            exit: true,
            migration: false,
        },
    );
    assert!(state.finished);
    assert_eq!(state.last_save, Some(SaveOutcome::Saved));
    assert!(!state.dirty());
}
#[test]
fn discard_never_saves_and_clean_exit_has_no_modal() {
    let mut state = state(json!({}));
    let effects = act(&mut state, Action::Exit);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, SetupEffect::Exit))
    );
    assert_eq!(state.last_save, None);
    let mut state = SetupState::new(state.draft);
    act(&mut state, Action::Toggle("skip_ads"));
    act(&mut state, Action::Exit);
    let effects = act(&mut state, Action::Discard);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, SetupEffect::Save { .. }))
    );
    assert!(state.finished);
}
#[test]
fn valid_pairing_is_guarded_by_code_generation_and_screen_identity() {
    let mut state = state(json!({}));
    let effects = act(&mut state, Action::AddDevice);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, SetupEffect::Discover { .. }))
    );
    assert!(state.hint().contains("launch YouTube"));
    let first = pair_stamp(&input(&mut state, Field::Code, "0000-1234 5678"));
    input(&mut state, Field::Code, "invalid");
    reduce(
        &mut state,
        SetupEvent::PairResult(first, Ok(Some(paired()))),
    );
    assert!(state.paired.is_none());
    act(&mut state, Action::AddPaired);
    assert!(state.values("devices").is_empty());
    let current = pair_stamp(&input(&mut state, Field::Code, "000012345678"));
    reduce(
        &mut state,
        SetupEvent::PairResult(current, Ok(Some(paired()))),
    );
    assert!(state.paired.is_some());
    act(&mut state, Action::Cancel);
    act(&mut state, Action::AddDevice);
    reduce(
        &mut state,
        SetupEvent::PairResult(current, Ok(Some(paired()))),
    );
    assert!(state.paired.is_none());
}
#[test]
fn pairing_add_uses_returned_name_and_never_persists_token() {
    let mut state = state(json!({}));
    act(&mut state, Action::AddDevice);
    let stamp = pair_stamp(&input(&mut state, Field::Code, "000012345678"));
    reduce(
        &mut state,
        SetupEvent::PairResult(stamp, Ok(Some(paired()))),
    );
    act(&mut state, Action::AddPaired);
    assert_eq!(state.screen, Screen::Main);
    assert_eq!(
        state.values("devices")[0],
        json!({"screen_id":"screen","name":"Living [room] 日本語","offset":0})
    );
}
#[test]
fn pairing_name_change_invalidates_pending_validation() {
    let mut state = state(json!({}));
    act(&mut state, Action::AddDevice);
    let first = pair_stamp(&input(&mut state, Field::Code, "000012345678"));
    let next = pair_stamp(&input(&mut state, Field::DeviceName, "Custom"));
    reduce(
        &mut state,
        SetupEvent::PairResult(first, Ok(Some(paired()))),
    );
    assert!(state.paired.is_none());
    reduce(&mut state, SetupEvent::PairResult(next, Ok(Some(paired()))));
    act(&mut state, Action::AddPaired);
    assert_eq!(state.values("devices")[0]["name"], "Custom");
}
#[test]
fn discovery_stream_deduplicates_selects_and_rejects_stale_retries() {
    let mut state = state(json!({"devices":[{"screen_id":"existing","name":"Saved"}]}));
    let first = discovery_stamp(&act(&mut state, Action::AddDevice));
    let device = FoundDevice {
        screen_id: "found".into(),
        name: "First".into(),
        selected: false,
    };
    reduce(&mut state, SetupEvent::DeviceFound(first, device.clone()));
    reduce(
        &mut state,
        SetupEvent::DeviceFound(
            first,
            FoundDevice {
                name: "Second".into(),
                ..device.clone()
            },
        ),
    );
    assert_eq!(state.discovered.len(), 1);
    assert_eq!(state.discovered[0].name, "First");
    let current = discovery_stamp(&act(&mut state, Action::Discover));
    assert!(state.discovered.is_empty());
    reduce(&mut state, SetupEvent::DeviceFound(first, device.clone()));
    assert!(state.discovered.is_empty());
    reduce(&mut state, SetupEvent::DeviceFound(current, device));
    reduce(&mut state, SetupEvent::DiscoveryComplete(current));
    assert!(state.discovery_status.starts_with("Complete"));
    act(&mut state, Action::DeviceTab(true));
    act(&mut state, Action::SelectDevice(0));
    act(&mut state, Action::AddDiscovered);
    assert_eq!(state.values("devices").len(), 2);
    assert_eq!(state.values("devices")[1]["offset"], 0);
}
#[test]
fn edit_preserves_fractional_offset_and_unknown_fields_until_slider_used() {
    let mut state = state(
        json!({"devices":[{"screen_id":"screen", "name":null, "offset":-250.5,"future":{"yes":true}}]}),
    );
    let original = state.draft.clone();
    act(&mut state, Action::EditDevice(0));
    reduce(&mut state, SetupEvent::Escape);
    assert_eq!(state.draft, original);
    act(&mut state, Action::EditDevice(0));
    reduce(&mut state, SetupEvent::Slider(1));
    reduce(&mut state, SetupEvent::Escape);
    assert_eq!(state.values("devices")[0]["offset"], 100);
    assert_eq!(state.values("devices")[0]["future"], json!({"yes":true}));
    assert!(state.values("devices")[0]["name"].is_null());
}
#[test]
fn invalid_device_cannot_close_and_signed_fractional_entry_can() {
    let mut state = state(json!({"devices":[{"screen_id":"screen","offset":2500.5}]}));
    act(&mut state, Action::EditDevice(0));
    input(&mut state, Field::Offset, "NaN");
    reduce(&mut state, SetupEvent::Escape);
    assert_eq!(state.screen, Screen::EditDevice);
    input(&mut state, Field::Offset, "-12.75");
    input(&mut state, Field::ScreenId, "");
    reduce(&mut state, SetupEvent::Escape);
    assert_eq!(state.screen, Screen::EditDevice);
    input(&mut state, Field::ScreenId, "replacement");
    reduce(&mut state, SetupEvent::Escape);
    assert_eq!(state.screen, Screen::Main);
    assert_eq!(state.values("devices")[0]["offset"], -12.75);
}
#[test]
fn no_results_and_no_key_keep_manual_channel_reachable() {
    let mut state = state(json!({}));
    act(&mut state, Action::AddChannel);
    assert!(state.hint().contains("requires API key"));
    input(&mut state, Field::Query, "query");
    assert!(
        !act(&mut state, Action::Search)
            .iter()
            .any(|effect| matches!(effect, SetupEffect::Search { .. }))
    );
    act(&mut state, Action::ChannelTab(true));
    input(&mut state, Field::ChannelId, "UCmanual");
    act(&mut state, Action::AddManual);
    assert_eq!(
        state.values("channel_whitelist")[0],
        json!({"id":"UCmanual","name":"UCmanual"})
    );
}
#[test]
fn channel_search_generation_empty_errors_and_selection() {
    let mut state = state(json!({"apikey":"key"}));
    act(&mut state, Action::AddChannel);
    input(&mut state, Field::Query, "query");
    let effects = act(&mut state, Action::Search);
    let stamp = effects
        .iter()
        .find_map(|effect| {
            if let SetupEffect::Search { stamp, .. } = effect {
                Some(*stamp)
            } else {
                None
            }
        })
        .unwrap();
    reduce(&mut state, SetupEvent::SearchResult(stamp, Ok(vec![])));
    assert!(state.search_status.contains("No results"));
    input(&mut state, Field::Query, "new");
    let channel = ChannelResult {
        id: "UCresult".into(),
        name: "Result".into(),
        subscribers: "Hidden".into(),
    };
    reduce(
        &mut state,
        SetupEvent::SearchResult(stamp, Ok(vec![channel.clone()])),
    );
    assert!(state.channels.is_empty());
    let effects = act(&mut state, Action::Search);
    let stamp = effects
        .iter()
        .find_map(|effect| {
            if let SetupEffect::Search { stamp, .. } = effect {
                Some(*stamp)
            } else {
                None
            }
        })
        .unwrap();
    reduce(
        &mut state,
        SetupEvent::SearchResult(stamp, Err("quota".into())),
    );
    assert!(state.message.contains("quota"));
    reduce(
        &mut state,
        SetupEvent::SearchResult(stamp, Ok(vec![channel])),
    );
    act(&mut state, Action::SelectChannel(0));
    act(&mut state, Action::AddFound);
    assert_eq!(state.values("channel_whitelist")[0]["id"], "UCresult");
}
#[test]
fn network_setting_changes_cancel_requests_and_preserve_draft() {
    let mut state = state(json!({}));
    for field in [Field::ApiKey, Field::BaseUrl] {
        let effects = input(
            &mut state,
            field,
            if field == Field::ApiKey {
                "new-key"
            } else {
                "https://example.test/api/"
            },
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SetupEffect::CancelAll))
        );
    }
    assert!(
        act(&mut state, Action::Toggle("use_proxy"))
            .iter()
            .any(|effect| matches!(effect, SetupEffect::CancelAll))
    );
    assert_eq!(state.draft.0["apikey"], "new-key");
    assert_eq!(state.draft.0["use_proxy"], true);
    input(&mut state, Field::Minimum, "1e-999");
    assert!(act(&mut state, Action::Save).is_empty());
    input(&mut state, Field::Minimum, "-1.5");
    assert!(
        act(&mut state, Action::Save)
            .iter()
            .any(|effect| matches!(effect, SetupEffect::Save { .. }))
    );
}
#[test]
fn shortcut_characters_are_text_only_while_editing() {
    let mut state = state(json!({}));
    focus(&mut state, Field::ApiKey);
    for character in ['s', 'q', ' '] {
        assert!(matches!(
            translate(
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                &state,
                &Surface::default()
            ),
            Some(SetupEvent::Edit(Edit::Insert(_)))
        ));
    }
    assert!(matches!(
        translate(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &state,
            &Surface::default()
        ),
        Some(SetupEvent::RequestExit { interrupted: true })
    ));
}
#[test]
fn all_screens_render_at_minimum_size_and_focus_stays_visible() {
    for (width, height) in [(40, 12), (80, 24), (140, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for screen in [
            Screen::Main,
            Screen::Migration,
            Screen::ConfirmExit,
            Screen::AddDevice,
            Screen::EditDevice,
            Screen::AddChannel,
        ] {
            let mut state =
                state(json!({"devices":[{"screen_id":"screen","name":"[red]界\u{001b}[2J"}]}));
            if screen == Screen::EditDevice {
                act(&mut state, Action::EditDevice(0));
            } else {
                state.screen = screen;
            }
            for alternate in [false, true] {
                state.lan_tab = alternate;
                state.manual_tab = alternate;
                for focus in 0..state.rows().len() {
                    state.focus = focus;
                    let mut surface = Surface::default();
                    terminal
                        .draw(|frame| surface = render(frame, &mut state))
                        .unwrap();
                    assert!(!surface.small);
                    assert!(surface.hits.iter().any(|hit| hit.row == focus));
                    for hit in surface.hits {
                        assert!(hit.area.right() <= width);
                        assert!(hit.area.bottom() <= height);
                    }
                    for cell in &terminal.backend().buffer().content {
                        assert!(!cell.symbol().chars().any(char::is_control));
                    }
                }
            }
        }
    }
}
#[test]
fn tiny_terminal_preserves_draft_and_handles_interrupt_resize() {
    let mut state = state(json!({"future":true}));
    let original = state.draft.clone();
    let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
    let mut surface = Surface::default();
    terminal
        .draw(|frame| surface = render(frame, &mut state))
        .unwrap();
    assert!(surface.small);
    assert_eq!(state.draft, original);
    assert!(
        translate(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            &state,
            &surface
        )
        .is_none()
    );
    assert!(
        translate(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &state,
            &surface
        )
        .is_some()
    );
    assert!(matches!(
        translate(Event::Resize(80, 24), &state, &surface),
        Some(SetupEvent::Focus(_))
    ));
}
#[test]
fn mouse_targets_recomputed_rows_and_outside_edit_commits() {
    let mut state = state(json!({"devices":[{"screen_id":"screen","offset":0}]}));
    act(&mut state, Action::EditDevice(0));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut surface = Surface::default();
    terminal
        .draw(|frame| surface = render(frame, &mut state))
        .unwrap();
    let first = &surface.hits[0];
    let mouse = |column, row| {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert!(matches!(
        translate(mouse(first.area.x, first.area.y), &state, &surface),
        Some(SetupEvent::Focus(0))
    ));
    let outside = translate(mouse(0, 0), &state, &surface).unwrap();
    reduce(&mut state, outside);
    assert_eq!(state.screen, Screen::Main);
}
#[test]
fn closing_modal_cancels_all_owned_requests() {
    let mut state = state(json!({}));
    act(&mut state, Action::AddDevice);
    assert!(
        act(&mut state, Action::Cancel)
            .iter()
            .any(|effect| matches!(effect, SetupEffect::CancelAll))
    );
    act(&mut state, Action::AddChannel);
    let effects = input(&mut state, Field::Query, "changed");
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, SetupEffect::Cancel(Request::Search)))
    );
}

#[test]
fn mouse_slider_sets_bounded_steps_without_touching_signed_input_before_click() {
    let mut state = state(json!({"devices":[{"screen_id":"screen","offset":-250.5}]}));
    act(&mut state, Action::EditDevice(0));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut surface = Surface::default();
    terminal
        .draw(|frame| surface = render(frame, &mut state))
        .unwrap();
    assert_eq!(state.input(Field::Offset).text, "-250.5");
    let hit = surface
        .hits
        .iter()
        .find(|hit| hit.action == Action::Slider)
        .unwrap();
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.area.right() - 1,
        row: hit.area.y,
        modifiers: KeyModifiers::NONE,
    });
    let event = translate(event, &state, &surface).unwrap();
    reduce(&mut state, event);
    assert_eq!(state.input(Field::Offset).text, "2000");
}

#[test]
fn cancelled_interrupt_does_not_turn_a_later_normal_exit_into_interrupt() {
    let mut state = state(json!({}));
    act(&mut state, Action::Toggle("mute_ads"));
    reduce(&mut state, SetupEvent::RequestExit { interrupted: true });
    assert!(state.interrupted);
    act(&mut state, Action::Cancel);
    assert!(!state.interrupted);
    act(&mut state, Action::Exit);
    act(&mut state, Action::Discard);
    assert!(!state.interrupted);
}

#[test]
fn deleting_between_regional_indicators_keeps_visual_cursor_boundary() {
    for deletion in [Edit::Delete, Edit::Backspace] {
        let mut input = TextInput::new("🇺x🇸");
        input.edit(Edit::Home);
        input.edit(Edit::Right);
        if matches!(deletion, Edit::Backspace) {
            input.edit(Edit::Right);
        }
        input.edit(deletion);
        assert_eq!(input.text, "🇺🇸");
        input.edit(Edit::Insert("A".into()));
        assert_eq!(input.text, "🇺🇸A");
    }
}

#[test]
fn invalid_device_can_interrupt_cancel_or_discard_but_cannot_save() {
    let mut state = state(json!({"devices":[{"screen_id":"screen","offset":0}]}));
    let original = state.draft.clone();
    act(&mut state, Action::EditDevice(0));
    input(&mut state, Field::Offset, "NaN");
    reduce(&mut state, SetupEvent::RequestExit { interrupted: true });
    assert_eq!(state.screen, Screen::ConfirmExit);
    assert!(act(&mut state, Action::SaveExit).is_empty());
    assert!(!state.finished);
    act(&mut state, Action::Cancel);
    assert_eq!(state.screen, Screen::EditDevice);
    assert_eq!(state.input(Field::Offset).text, "NaN");
    assert!(!state.interrupted);
    reduce(&mut state, SetupEvent::RequestExit { interrupted: true });
    let event = translate(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &state,
        &Surface::default(),
    )
    .unwrap();
    reduce(&mut state, event);
    assert!(state.finished);
    assert!(state.interrupted);
    assert_eq!(state.draft, original);
}
