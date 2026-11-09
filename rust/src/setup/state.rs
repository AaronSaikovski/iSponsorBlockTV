//! Pure setup state: the persisted JSON remains lossless; effects own all I/O.
use super::{
    cli::{finite_number, valid_base_url},
    widgets::{Edit, Row, TextInput},
};
use crate::{
    api::{ChannelResult, PairingResult},
    config::{ConfigDocument, DEFAULT_SPONSORBLOCK_API_URL, SaveOutcome},
};
use serde_json::{Value, json};

pub const CATEGORIES: [&str; 10] = [
    "sponsor",
    "selfpromo",
    "intro",
    "outro",
    "music_offtopic",
    "interaction",
    "exclusive_access",
    "preview",
    "filler",
    "hook",
];
pub const CREDENTIALS_URL: &str =
    "https://developers.google.com/youtube/registering_an_application";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Main,
    Migration,
    ConfirmExit,
    AddDevice,
    EditDevice,
    AddChannel,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    ApiKey,
    Minimum,
    BaseUrl,
    Code,
    DeviceName,
    ScreenId,
    Offset,
    Query,
    ChannelId,
    ChannelName,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Pair,
    Search,
    Discovery,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub screen: u64,
    pub generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Input(Field),
    AddDevice,
    EditDevice(usize),
    RemoveDevice(usize),
    Category(String),
    Toggle(&'static str),
    AddChannel,
    RemoveChannel(usize),
    ShowKey,
    Credentials,
    Save,
    Exit,
    Migrate,
    Keep,
    SaveExit,
    Discard,
    Cancel,
    DeviceTab(bool),
    ValidateCode,
    AddPaired,
    Discover,
    SelectDevice(usize),
    AddDiscovered,
    ShowId,
    Slider,
    CommitDevice,
    ChannelTab(bool),
    Search,
    SelectChannel(usize),
    AddFound,
    AddManual,
}
#[derive(Clone, Debug)]
pub struct FoundDevice {
    pub screen_id: String,
    pub name: String,
    pub selected: bool,
}
#[derive(Clone, Debug)]
pub enum SetupEvent {
    Activate(Action),
    Click(usize, Action),
    Focus(usize),
    Move(isize),
    Edit(Edit),
    Slider(i32),
    SliderTo(usize, u16),
    Escape,
    RequestExit {
        interrupted: bool,
    },
    PairResult(Stamp, Result<Option<PairingResult>, String>),
    SearchResult(Stamp, Result<Vec<ChannelResult>, String>),
    DeviceFound(Stamp, FoundDevice),
    DiscoveryWarning(Stamp, String),
    DiscoveryComplete(Stamp),
    Saved {
        result: Result<SaveOutcome, String>,
        exit: bool,
        migration: bool,
    },
    Message(String),
}
#[derive(Clone, Debug)]
pub enum SetupEffect {
    Cancel(Request),
    CancelAll,
    Pair { stamp: Stamp, code: String },
    Search { stamp: Stamp, query: String },
    Discover { stamp: Stamp },
    Save { exit: bool, migration: bool },
    OpenCredentials,
    Exit,
}

pub struct SetupState {
    pub baseline: ConfigDocument,
    pub draft: ConfigDocument,
    pub screen: Screen,
    pub instance: u64,
    pub focus: usize,
    pub scroll: usize,
    pub message: String,
    pub show_key: bool,
    pub show_id: bool,
    pub lan_tab: bool,
    pub manual_tab: bool,
    pub pair_generation: u64,
    pub search_generation: u64,
    pub discovery_generation: u64,
    pub paired: Option<PairingResult>,
    pub pairing_status: String,
    pub search_status: String,
    pub discovery_status: String,
    pub discovered: Vec<FoundDevice>,
    pub channels: Vec<ChannelResult>,
    pub selected_channel: Option<usize>,
    pub edited_device: Option<usize>,
    pub last_save: Option<SaveOutcome>,
    pub interrupted: bool,
    pub finished: bool,
    exit_from: Option<Screen>,
    api_key: TextInput,
    minimum: TextInput,
    base_url: TextInput,
    code: TextInput,
    device_name: TextInput,
    screen_id: TextInput,
    offset: TextInput,
    query: TextInput,
    channel_id: TextInput,
    channel_name: TextInput,
}

// The layout borrows category names and formatting arguments. Only rendering
// owns labels; focus and navigation consume the same ordering without them.
enum RowAction<'a> {
    Button(Action),
    Category(&'a str),
}

impl RowAction<'_> {
    fn into_action(self) -> Action {
        match self {
            Self::Button(action) => action,
            Self::Category(category) => Action::Category(category.to_owned()),
        }
    }
}

struct RowSpec<'a> {
    label: std::fmt::Arguments<'a>,
    action: RowAction<'a>,
    field: Option<Field>,
    masked: bool,
}

impl<'a> RowSpec<'a> {
    fn button(label: std::fmt::Arguments<'a>, action: Action) -> Self {
        Self {
            label,
            action: RowAction::Button(action),
            field: None,
            masked: false,
        }
    }

    fn input(label: std::fmt::Arguments<'a>, field: Field, masked: bool) -> Self {
        Self {
            label,
            action: RowAction::Button(Action::Input(field)),
            field: Some(field),
            masked,
        }
    }

    fn category(label: std::fmt::Arguments<'a>, category: &'a str) -> Self {
        Self {
            label,
            action: RowAction::Category(category),
            field: None,
            masked: false,
        }
    }
}

impl SetupState {
    pub fn new(document: ConfigDocument) -> Self {
        let text = |key: &str, fallback: &str| {
            document
                .0
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or(fallback)
                .to_owned()
        };
        let minimum = document
            .0
            .get("minimum_skip_length")
            .map_or_else(|| "1".into(), Value::to_string);
        let screen = if document.0.contains_key("atvs") {
            Screen::Migration
        } else {
            Screen::Main
        };
        Self {
            api_key: TextInput::new(text("apikey", "")),
            minimum: TextInput::new(minimum),
            base_url: TextInput::new(text("sponsorblock_api_url", DEFAULT_SPONSORBLOCK_API_URL)),
            baseline: document.clone(),
            draft: document,
            screen,
            instance: 1,
            focus: 0,
            scroll: 0,
            message: String::new(),
            show_key: false,
            show_id: false,
            lan_tab: false,
            manual_tab: false,
            pair_generation: 0,
            search_generation: 0,
            discovery_generation: 0,
            paired: None,
            pairing_status: "Enter the twelve-digit TV code".into(),
            search_status: "Enter a channel name and Search".into(),
            discovery_status: String::new(),
            discovered: Vec::new(),
            channels: Vec::new(),
            selected_channel: None,
            edited_device: None,
            last_save: None,
            interrupted: false,
            finished: false,
            exit_from: None,
            code: TextInput::default(),
            device_name: TextInput::default(),
            screen_id: TextInput::default(),
            offset: TextInput::default(),
            query: TextInput::default(),
            channel_id: TextInput::default(),
            channel_name: TextInput::default(),
        }
    }
    pub fn dirty(&self) -> bool {
        self.draft != self.baseline || !self.main_inputs_valid()
    }
    pub fn title(&self) -> &'static str {
        match self.screen {
            Screen::Main => "Setup",
            Screen::Migration => "Legacy configuration",
            Screen::ConfirmExit => "Save changes?",
            Screen::AddDevice => "Add device",
            Screen::EditDevice => "Edit device",
            Screen::AddChannel => "Add whitelist channel",
        }
    }
    pub fn input(&self, field: Field) -> &TextInput {
        match field {
            Field::ApiKey => &self.api_key,
            Field::Minimum => &self.minimum,
            Field::BaseUrl => &self.base_url,
            Field::Code => &self.code,
            Field::DeviceName => &self.device_name,
            Field::ScreenId => &self.screen_id,
            Field::Offset => &self.offset,
            Field::Query => &self.query,
            Field::ChannelId => &self.channel_id,
            Field::ChannelName => &self.channel_name,
        }
    }
    fn input_mut(&mut self, field: Field) -> &mut TextInput {
        match field {
            Field::ApiKey => &mut self.api_key,
            Field::Minimum => &mut self.minimum,
            Field::BaseUrl => &mut self.base_url,
            Field::Code => &mut self.code,
            Field::DeviceName => &mut self.device_name,
            Field::ScreenId => &mut self.screen_id,
            Field::Offset => &mut self.offset,
            Field::Query => &mut self.query,
            Field::ChannelId => &mut self.channel_id,
            Field::ChannelName => &mut self.channel_name,
        }
    }
    pub fn focused_action(&self) -> Option<Action> {
        let mut index = 0;
        let mut action = None;
        self.visit_rows(|row| {
            if index == self.focus {
                action = Some(row.action.into_action());
            }
            index += 1;
        });
        action
    }
    pub fn focused_field(&self) -> Option<Field> {
        let mut index = 0;
        let mut field = None;
        self.visit_rows(|row| {
            if index == self.focus {
                field = row.field;
            }
            index += 1;
        });
        field
    }
    pub fn values(&self, key: &str) -> &[Value] {
        self.draft
            .0
            .get(key)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }
    pub fn categories(&self) -> Vec<String> {
        match self.draft.0.get("skip_categories") {
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => vec!["sponsor".into()],
        }
    }
    pub fn boolean(&self, key: &str) -> bool {
        self.draft
            .0
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(matches!(key, "skip_count_tracking" | "auto_play"))
    }
    pub fn hint(&self) -> String {
        match self.screen {
            Screen::Main if self.draft.0.contains_key("atvs") => {
                "Legacy atvs entry blocks start; reopen setup to migrate".into()
            }
            Screen::Main
                if self.api_key.text.is_empty() && !self.values("channel_whitelist").is_empty() =>
            {
                "Whitelist needs a YouTube API key before start".into()
            }
            Screen::Main => "Settings are drafts until explicitly saved".into(),
            Screen::Migration => "Legacy atvs blocks start. Removal is explicit and saved.".into(),
            Screen::ConfirmExit => "Save the draft, discard edits, or return to setup".into(),
            Screen::AddDevice => {
                "Active LAN search can launch YouTube (also on pairing tab)".into()
            }
            Screen::EditDevice => "Escape / outside click commits valid edits to draft".into(),
            Screen::AddChannel if self.api_key.text.is_empty() => {
                "Search requires API key; manual entry is available".into()
            }
            Screen::AddChannel => "Whitelisted channels bypass SponsorBlock segments only".into(),
        }
    }
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.visit_rows(|row| {
            rows.push(Row {
                label: row.label.to_string(),
                action: row.action.into_action(),
                field: row.field,
                masked: row.masked,
            });
        });
        rows
    }

    pub(crate) fn row_count(&self) -> usize {
        let mut count = 0;
        self.visit_rows(|_| count += 1);
        count
    }

    fn visit_rows(&self, mut visit: impl FnMut(RowSpec<'_>)) {
        match self.screen {
            Screen::Main => {
                visit(RowSpec::button(
                    format_args!("Add device"),
                    Action::AddDevice,
                ));
                for (i, device) in self.values("devices").iter().enumerate() {
                    let name = device
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Unnamed device");
                    visit(RowSpec::button(
                        format_args!("Edit device: {name}"),
                        Action::EditDevice(i),
                    ));
                    visit(RowSpec::button(
                        format_args!("Remove device: {name}"),
                        Action::RemoveDevice(i),
                    ));
                }
                let selected = self
                    .draft
                    .0
                    .get("skip_categories")
                    .and_then(Value::as_array);
                let mut category_row = |category: &str| {
                    let checked = selected.map_or(category == "sponsor", |values| {
                        values.iter().any(|value| value.as_str() == Some(category))
                    });
                    visit(RowSpec::category(
                        format_args!("[{}] Skip {category}", if checked { "x" } else { " " }),
                        category,
                    ));
                };
                for category in CATEGORIES {
                    category_row(category);
                }
                if let Some(selected) = selected {
                    for (index, value) in selected.iter().enumerate() {
                        if let Some(category) = value.as_str()
                            && !CATEGORIES.contains(&category)
                            && !selected[..index]
                                .iter()
                                .any(|value| value.as_str() == Some(category))
                        {
                            category_row(category);
                        }
                    }
                }
                visit(RowSpec::input(
                    format_args!("Minimum skip seconds"),
                    Field::Minimum,
                    false,
                ));
                for (key, label) in [
                    ("skip_count_tracking", "Report skipped segments"),
                    ("mute_ads", "Mute YouTube ads"),
                    ("skip_ads", "Skip YouTube ads"),
                ] {
                    visit(RowSpec::button(
                        format_args!("[{}] {label}", if self.boolean(key) { "x" } else { " " }),
                        Action::Toggle(key),
                    ));
                }
                visit(RowSpec::button(
                    format_args!("Add whitelist channel"),
                    Action::AddChannel,
                ));
                for (i, channel) in self.values("channel_whitelist").iter().enumerate() {
                    let name = channel
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| channel.get("id").and_then(Value::as_str))
                        .unwrap_or("Unnamed channel");
                    visit(RowSpec::button(
                        format_args!("Remove channel: {name}"),
                        Action::RemoveChannel(i),
                    ));
                }
                visit(RowSpec::input(
                    format_args!("YouTube API key"),
                    Field::ApiKey,
                    !self.show_key,
                ));
                visit(RowSpec::button(
                    format_args!(
                        "{}",
                        if self.show_key {
                            "Hide API key"
                        } else {
                            "Show API key"
                        }
                    ),
                    Action::ShowKey,
                ));
                visit(RowSpec::button(
                    format_args!("Get YouTube API credentials (browser)"),
                    Action::Credentials,
                ));
                for (key, label) in [("auto_play", "Autoplay"), ("use_proxy", "Use system proxy")] {
                    visit(RowSpec::button(
                        format_args!("[{}] {label}", if self.boolean(key) { "x" } else { " " }),
                        Action::Toggle(key),
                    ));
                }
                visit(RowSpec::input(
                    format_args!("SponsorBlock API URL"),
                    Field::BaseUrl,
                    false,
                ));
                visit(RowSpec::button(
                    format_args!("Save configuration"),
                    Action::Save,
                ));
                visit(RowSpec::button(format_args!("Exit"), Action::Exit));
            }
            Screen::Migration => {
                visit(RowSpec::button(
                    format_args!("Remove atvs and save"),
                    Action::Migrate,
                ));
                visit(RowSpec::button(format_args!("Keep atvs"), Action::Keep));
            }
            Screen::ConfirmExit => {
                visit(RowSpec::button(
                    format_args!("Save and exit"),
                    Action::SaveExit,
                ));
                visit(RowSpec::button(
                    format_args!("Discard and exit"),
                    Action::Discard,
                ));
                visit(RowSpec::button(format_args!("Cancel"), Action::Cancel));
            }
            Screen::AddDevice => {
                visit(RowSpec::button(
                    format_args!(
                        "{}",
                        if self.lan_tab {
                            "Pairing code tab"
                        } else {
                            "[Pairing code tab]"
                        }
                    ),
                    Action::DeviceTab(false),
                ));
                visit(RowSpec::button(
                    format_args!(
                        "{}",
                        if self.lan_tab {
                            "[LAN discovery tab]"
                        } else {
                            "LAN discovery tab"
                        }
                    ),
                    Action::DeviceTab(true),
                ));
                if self.lan_tab {
                    visit(RowSpec::button(
                        format_args!("Start search / retry — {}", self.discovery_status),
                        Action::Discover,
                    ));
                    for (i, device) in self.discovered.iter().enumerate() {
                        visit(RowSpec::button(
                            format_args!(
                                "[{}] {}",
                                if device.selected { "x" } else { " " },
                                device.name
                            ),
                            Action::SelectDevice(i),
                        ));
                    }
                    visit(RowSpec::button(
                        format_args!("Add selected devices"),
                        Action::AddDiscovered,
                    ));
                } else {
                    visit(RowSpec::input(
                        format_args!("Twelve-digit TV code"),
                        Field::Code,
                        false,
                    ));
                    visit(RowSpec::input(
                        format_args!("Name (optional)"),
                        Field::DeviceName,
                        false,
                    ));
                    visit(RowSpec::button(
                        format_args!("Validate / retry — {}", self.pairing_status),
                        Action::ValidateCode,
                    ));
                    visit(RowSpec::button(
                        format_args!(
                            "{}",
                            if self.paired.is_some() {
                                "Add paired device"
                            } else {
                                "Add (validate code first)"
                            }
                        ),
                        Action::AddPaired,
                    ));
                }
                visit(RowSpec::button(format_args!("Cancel"), Action::Cancel));
            }
            Screen::EditDevice => {
                visit(RowSpec::input(
                    format_args!("Name"),
                    Field::DeviceName,
                    false,
                ));
                visit(RowSpec::input(
                    format_args!("Screen ID"),
                    Field::ScreenId,
                    !self.show_id,
                ));
                visit(RowSpec::button(
                    format_args!(
                        "{}",
                        if self.show_id {
                            "Hide screen ID"
                        } else {
                            "Show screen ID"
                        }
                    ),
                    Action::ShowId,
                ));
                visit(RowSpec::input(
                    format_args!("Offset milliseconds"),
                    Field::Offset,
                    false,
                ));
                visit(RowSpec::button(
                    format_args!("Offset slider [0..2000] {} ms (←/→ 100)", self.offset.text),
                    Action::Slider,
                ));
                visit(RowSpec::button(
                    format_args!("Apply and close"),
                    Action::CommitDevice,
                ));
            }
            Screen::AddChannel => {
                visit(RowSpec::button(
                    format_args!(
                        "{}",
                        if self.manual_tab {
                            "Search tab"
                        } else {
                            "[Search tab]"
                        }
                    ),
                    Action::ChannelTab(false),
                ));
                visit(RowSpec::button(
                    format_args!(
                        "{}",
                        if self.manual_tab {
                            "[Manual ID tab]"
                        } else {
                            "Manual ID tab"
                        }
                    ),
                    Action::ChannelTab(true),
                ));
                if self.manual_tab {
                    visit(RowSpec::input(
                        format_args!("Channel ID"),
                        Field::ChannelId,
                        false,
                    ));
                    visit(RowSpec::input(
                        format_args!("Name (optional)"),
                        Field::ChannelName,
                        false,
                    ));
                    visit(RowSpec::button(
                        format_args!("Add channel"),
                        Action::AddManual,
                    ));
                } else {
                    visit(RowSpec::input(
                        format_args!("Channel name query"),
                        Field::Query,
                        false,
                    ));
                    visit(RowSpec::button(
                        format_args!("Search / retry — {}", self.search_status),
                        Action::Search,
                    ));
                    for (i, channel) in self.channels.iter().enumerate() {
                        visit(RowSpec::button(
                            format_args!(
                                "({}) {} — Subs: {}",
                                if self.selected_channel == Some(i) {
                                    "x"
                                } else {
                                    " "
                                },
                                channel.name,
                                channel.subscribers
                            ),
                            Action::SelectChannel(i),
                        ));
                    }
                    visit(RowSpec::button(
                        format_args!("Add selected channel"),
                        Action::AddFound,
                    ));
                }
                visit(RowSpec::button(format_args!("Cancel"), Action::Cancel));
            }
        }
    }
    fn stamp(&self, request: Request) -> Stamp {
        Stamp {
            screen: self.instance,
            generation: match request {
                Request::Pair => self.pair_generation,
                Request::Search => self.search_generation,
                Request::Discovery => self.discovery_generation,
            },
        }
    }
    fn current(&self, stamp: Stamp, request: Request) -> bool {
        self.stamp(request) == stamp
            && self.screen
                == if request == Request::Search {
                    Screen::AddChannel
                } else {
                    Screen::AddDevice
                }
    }
    fn open(&mut self, screen: Screen) {
        self.instance += 1;
        self.pair_generation += 1;
        self.search_generation += 1;
        self.discovery_generation += 1;
        self.screen = screen;
        self.focus = 0;
        self.scroll = 0;
        self.message.clear();
        self.paired = None;
    }
    fn main_inputs_valid(&self) -> bool {
        finite_number(self.minimum.text.trim()).is_some()
            && valid_base_url(self.base_url.text.trim())
    }
    fn validate_save(&mut self) -> bool {
        if !self.main_inputs_valid() {
            self.message = "Correct minimum seconds and SponsorBlock URL before saving".into();
            false
        } else {
            true
        }
    }
    fn append(&mut self, key: &str, value: Value) -> bool {
        let entry = self
            .draft
            .0
            .entry(key.to_owned())
            .or_insert_with(|| json!([]));
        if let Some(values) = entry.as_array_mut() {
            values.push(value);
            true
        } else {
            self.message = format!("{key} must be a JSON array; existing data was not overwritten");
            false
        }
    }
    fn remove(&mut self, key: &str, index: usize) {
        if let Some(values) = self.draft.0.get_mut(key).and_then(Value::as_array_mut)
            && index < values.len()
        {
            values.remove(index);
        }
    }
}

fn close(state: &mut SetupState) -> Vec<SetupEffect> {
    let screen = if state.screen == Screen::ConfirmExit {
        state.interrupted = false;
        state.exit_from.take().unwrap_or(Screen::Main)
    } else {
        Screen::Main
    };
    state.open(screen);
    vec![SetupEffect::CancelAll]
}
fn discover(state: &mut SetupState) -> Vec<SetupEffect> {
    state.discovery_generation += 1;
    state.discovered.clear();
    state.discovery_status = "Searching…".into();
    vec![
        SetupEffect::Cancel(Request::Discovery),
        SetupEffect::Discover {
            stamp: state.stamp(Request::Discovery),
        },
    ]
}
fn pair(state: &mut SetupState) -> Vec<SetupEffect> {
    state.pair_generation += 1;
    state.paired = None;
    let code: String = state
        .code
        .text
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .collect();
    let mut effects = vec![SetupEffect::Cancel(Request::Pair)];
    if code.len() != 12 || !code.bytes().all(|c| c.is_ascii_digit()) {
        state.pairing_status = "Enter exactly twelve ASCII digits".into();
    } else {
        state.pairing_status = "Validating…".into();
        effects.push(SetupEffect::Pair {
            stamp: state.stamp(Request::Pair),
            code,
        });
    }
    effects
}
fn commit_device(state: &mut SetupState) -> bool {
    let Some(offset) = finite_number(state.offset.text.trim()) else {
        state.message = "Offset must be a finite signed/fractional millisecond number".into();
        return false;
    };
    if state.screen_id.text.trim().is_empty() {
        state.message = "Screen ID cannot be empty".into();
        return false;
    }
    let Some(index) = state.edited_device else {
        return false;
    };
    let Some(device) = state
        .draft
        .0
        .get_mut("devices")
        .and_then(Value::as_array_mut)
        .and_then(|d| d.get_mut(index))
        .and_then(Value::as_object_mut)
    else {
        state.message = "Device record must be a JSON object".into();
        return false;
    };
    // Preserve absent/null optional fields unless actually edited.
    if device.get("name").and_then(Value::as_str).unwrap_or("") != state.device_name.text {
        device.insert("name".into(), json!(state.device_name.text));
    }
    if device
        .get("screen_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        != state.screen_id.text
    {
        device.insert("screen_id".into(), json!(state.screen_id.text));
    }
    let previous = device
        .get("offset")
        .map_or_else(|| "0".into(), Value::to_string);
    if previous != state.offset.text {
        device.insert("offset".into(), offset);
    }
    true
}

pub fn reduce(state: &mut SetupState, event: SetupEvent) -> Vec<SetupEffect> {
    match event {
        SetupEvent::Focus(index) => {
            state.focus = index.min(state.row_count().saturating_sub(1));
        }
        SetupEvent::Click(index, action) => {
            state.focus = index.min(state.row_count().saturating_sub(1));
            return activate(state, action);
        }
        SetupEvent::Move(delta) => {
            let count = state.row_count();
            if count > 0 {
                state.focus = (state.focus as isize + delta).rem_euclid(count as isize) as usize;
            }
        }
        SetupEvent::Edit(edit) => {
            let Some(field) = state.focused_field() else {
                return vec![];
            };
            if !state.input_mut(field).edit(edit) {
                return vec![];
            }
            state.message.clear();
            match field {
                Field::ApiKey => {
                    state
                        .draft
                        .0
                        .insert("apikey".into(), json!(state.api_key.text));
                    state.search_generation += 1;
                    return vec![SetupEffect::CancelAll];
                }
                Field::Minimum => {
                    if let Some(number) = finite_number(state.minimum.text.trim()) {
                        state.draft.0.insert("minimum_skip_length".into(), number);
                    } else {
                        state.message = "Minimum skip seconds must be a finite number".into();
                    }
                }
                Field::BaseUrl => {
                    state.pair_generation += 1;
                    state.search_generation += 1;
                    state.discovery_generation += 1;
                    if valid_base_url(state.base_url.text.trim()) {
                        state.draft.0.insert(
                            "sponsorblock_api_url".into(),
                            json!(state.base_url.text.trim()),
                        );
                    } else {
                        state.message =
                            "Use an HTTP(S) base URL without credentials, query or fragment".into();
                    }
                    return vec![SetupEffect::CancelAll];
                }
                Field::Code | Field::DeviceName if state.screen == Screen::AddDevice => {
                    return pair(state);
                }
                Field::Query | Field::ChannelId | Field::ChannelName => {
                    state.search_generation += 1;
                    state.channels.clear();
                    state.selected_channel = None;
                    state.search_status = "Input changed; search again".into();
                    return vec![SetupEffect::Cancel(Request::Search)];
                }
                _ => {}
            }
        }
        SetupEvent::Slider(delta) if state.screen == Screen::EditDevice => {
            let value = state
                .offset
                .text
                .parse::<f64>()
                .unwrap_or(0.0)
                .clamp(0.0, 2000.0);
            let value = ((value / 100.0).round() as i32 * 100 + delta * 100).clamp(0, 2000);
            state.offset = TextInput::new(value.to_string());
            state.message.clear();
        }
        SetupEvent::SliderTo(index, value) if state.screen == Screen::EditDevice => {
            state.focus = index;
            state.offset = TextInput::new((value.min(2000) / 100 * 100).to_string());
            state.message.clear();
        }
        SetupEvent::Slider(_) | SetupEvent::SliderTo(..) => {}
        SetupEvent::Escape => {
            if state.screen == Screen::Main {
                return reduce(state, SetupEvent::RequestExit { interrupted: false });
            }
            if state.screen == Screen::EditDevice && !commit_device(state) {
                return vec![];
            }
            return close(state);
        }
        SetupEvent::RequestExit { interrupted } => {
            state.interrupted |= interrupted;
            let invalid_device = state.screen == Screen::EditDevice && !commit_device(state);
            if invalid_device || state.dirty() {
                state.exit_from = Some(state.screen);
                state.open(Screen::ConfirmExit);
                return vec![SetupEffect::CancelAll];
            }
            state.finished = true;
            return vec![SetupEffect::CancelAll, SetupEffect::Exit];
        }
        SetupEvent::PairResult(stamp, result) if state.current(stamp, Request::Pair) => {
            match result {
                Ok(Some(device)) if !device.screen_id.is_empty() => {
                    state.pairing_status = format!(
                        "Valid — {}",
                        device.name.as_deref().unwrap_or("Unnamed device")
                    );
                    state.paired = Some(device);
                }
                Ok(_) => {
                    state.paired = None;
                    state.pairing_status =
                        "Rejected; check code or restart YouTube, then retry".into();
                }
                Err(error) => {
                    state.paired = None;
                    state.pairing_status = "Error; retry available".into();
                    state.message = error;
                }
            }
        }
        SetupEvent::SearchResult(stamp, result) if state.current(stamp, Request::Search) => {
            match result {
                Ok(channels) => {
                    state.search_status = if channels.is_empty() {
                        "No results; retry or use Manual ID".into()
                    } else {
                        format!("{} results", channels.len())
                    };
                    state.channels = channels;
                    state.selected_channel = None;
                }
                Err(error) => {
                    state.search_status = "Error; retry or use Manual ID".into();
                    state.message = error;
                }
            }
        }
        SetupEvent::DeviceFound(stamp, device) if state.current(stamp, Request::Discovery) => {
            if !state
                .discovered
                .iter()
                .any(|existing| existing.screen_id == device.screen_id)
            {
                state.discovered.push(device);
            }
        }
        SetupEvent::DiscoveryWarning(stamp, warning)
            if state.current(stamp, Request::Discovery) =>
        {
            state.discovery_status = "Backend warning; other discovery continues".into();
            state.message = warning;
        }
        SetupEvent::DiscoveryComplete(stamp) if state.current(stamp, Request::Discovery) => {
            state.discovery_status = if state.discovered.is_empty() {
                "Complete; no devices found. Retry or pair with code".into()
            } else {
                format!("Complete; {} devices", state.discovered.len())
            };
        }
        SetupEvent::PairResult(..)
        | SetupEvent::SearchResult(..)
        | SetupEvent::DeviceFound(..)
        | SetupEvent::DiscoveryWarning(..)
        | SetupEvent::DiscoveryComplete(..) => {}
        SetupEvent::Message(message) => state.message = message,
        SetupEvent::Saved {
            result,
            exit,
            migration,
        } => match result {
            Ok(outcome) => {
                state.baseline = state.draft.clone();
                state.last_save = Some(outcome);
                state.message = match outcome {
                    SaveOutcome::Saved => "Configuration saved",
                    SaveOutcome::Unchanged => "Configuration unchanged",
                }
                .into();
                if exit {
                    state.interrupted = false;
                    state.finished = true;
                    return vec![SetupEffect::CancelAll, SetupEffect::Exit];
                }
                if migration {
                    return close(state);
                }
            }
            Err(message) => {
                state.message = format!("Save failed: {message}. Draft retained; retry or cancel.");
            }
        },
        SetupEvent::Activate(action) => return activate(state, action),
    }
    vec![]
}

fn activate(state: &mut SetupState, action: Action) -> Vec<SetupEffect> {
    match action {
        Action::Input(Field::Code) | Action::Input(Field::DeviceName)
            if state.screen == Screen::AddDevice =>
        {
            return activate(
                state,
                if state.paired.is_some() {
                    Action::AddPaired
                } else {
                    Action::ValidateCode
                },
            );
        }
        Action::Input(Field::Query) => return activate(state, Action::Search),
        Action::Input(Field::ChannelId) | Action::Input(Field::ChannelName) => {
            return activate(state, Action::AddManual);
        }
        Action::Input(_) if state.screen == Screen::EditDevice => {
            return activate(state, Action::CommitDevice);
        }
        Action::Input(_) => {}
        Action::AddDevice => {
            state.open(Screen::AddDevice);
            state.lan_tab = false;
            state.code = TextInput::default();
            state.device_name = TextInput::default();
            state.pairing_status = "Enter the twelve-digit TV code".into();
            let mut effects = vec![SetupEffect::CancelAll];
            effects.extend(discover(state));
            return effects;
        }
        Action::EditDevice(index) => {
            let Some(device) = state.values("devices").get(index) else {
                return vec![];
            };
            let name = device
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let id = device
                .get("screen_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let offset = device
                .get("offset")
                .map_or_else(|| "0".into(), Value::to_string);
            state.open(Screen::EditDevice);
            state.edited_device = Some(index);
            state.show_id = false;
            state.device_name = TextInput::new(name);
            state.screen_id = TextInput::new(id);
            state.offset = TextInput::new(offset);
        }
        Action::RemoveDevice(index) => state.remove("devices", index),
        Action::Category(category) => {
            let mut values = state.categories();
            if values.contains(&category) {
                values.retain(|value| value != &category);
            } else {
                values.push(category);
            }
            state
                .draft
                .0
                .insert("skip_categories".into(), json!(values));
        }
        Action::Toggle(key) => {
            state.draft.0.insert(key.into(), json!(!state.boolean(key)));
            if key == "use_proxy" {
                state.pair_generation += 1;
                state.search_generation += 1;
                state.discovery_generation += 1;
                return vec![SetupEffect::CancelAll];
            }
        }
        Action::AddChannel => {
            state.open(Screen::AddChannel);
            state.manual_tab = false;
            state.query = TextInput::default();
            state.channel_id = TextInput::default();
            state.channel_name = TextInput::default();
            state.channels.clear();
            state.selected_channel = None;
            state.search_status = "Enter query".into();
            return vec![SetupEffect::CancelAll];
        }
        Action::RemoveChannel(index) => state.remove("channel_whitelist", index),
        Action::ShowKey => state.show_key = !state.show_key,
        Action::Credentials => return vec![SetupEffect::OpenCredentials],
        Action::Save | Action::SaveExit => {
            if (state.screen == Screen::EditDevice || state.exit_from == Some(Screen::EditDevice))
                && !commit_device(state)
            {
                return vec![];
            }
            if state.validate_save() {
                return vec![SetupEffect::Save {
                    exit: action == Action::SaveExit,
                    migration: false,
                }];
            }
        }
        Action::Exit => return reduce(state, SetupEvent::RequestExit { interrupted: false }),
        Action::Migrate => {
            state.draft.0.remove("atvs");
            return vec![SetupEffect::Save {
                exit: false,
                migration: true,
            }];
        }
        Action::Keep | Action::Cancel => return reduce(state, SetupEvent::Escape),
        Action::Discard => {
            state.finished = true;
            return vec![SetupEffect::CancelAll, SetupEffect::Exit];
        }
        Action::DeviceTab(lan) => {
            state.lan_tab = lan;
            state.focus = 0;
            state.scroll = 0;
        }
        Action::ValidateCode => return pair(state),
        Action::AddPaired => {
            let Some(device) = state.paired.clone() else {
                state.message = "Validate the current twelve-digit code before adding".into();
                return vec![];
            };
            let name = if state.device_name.text.is_empty() {
                device.name
            } else {
                Some(state.device_name.text.clone())
            };
            if state.append(
                "devices",
                json!({"screen_id":device.screen_id,"name":name,"offset":0}),
            ) {
                return close(state);
            }
        }
        Action::Discover => return discover(state),
        Action::SelectDevice(index) => {
            if let Some(device) = state.discovered.get_mut(index) {
                device.selected = !device.selected;
            }
        }
        Action::AddDiscovered => {
            let devices: Vec<Value> = state
                .discovered
                .iter()
                .filter(|device| device.selected)
                .map(|device| json!({"screen_id":device.screen_id,"name":device.name,"offset":0}))
                .collect();
            if devices.is_empty() {
                state.message = "Select at least one discovered device".into();
                return vec![];
            }
            for device in devices {
                if !state.append("devices", device) {
                    return vec![];
                }
            }
            return close(state);
        }
        Action::ShowId => state.show_id = !state.show_id,
        Action::Slider => return reduce(state, SetupEvent::Slider(1)),
        Action::CommitDevice => {
            if commit_device(state) {
                return close(state);
            }
        }
        Action::ChannelTab(manual) => {
            state.manual_tab = manual;
            state.focus = 0;
            state.scroll = 0;
            state.search_generation += 1;
            state.channels.clear();
            state.selected_channel = None;
            return vec![SetupEffect::Cancel(Request::Search)];
        }
        Action::Search => {
            state.search_generation += 1;
            state.channels.clear();
            state.selected_channel = None;
            let mut effects = vec![SetupEffect::Cancel(Request::Search)];
            if state.api_key.text.is_empty() {
                state.message =
                    "Set a YouTube API key on the main screen, or use manual entry".into();
            } else if state.query.text.trim().is_empty() {
                state.message = "Enter a channel name".into();
            } else {
                state.search_status = "Searching…".into();
                effects.push(SetupEffect::Search {
                    stamp: state.stamp(Request::Search),
                    query: state.query.text.trim().into(),
                });
            }
            return effects;
        }
        Action::SelectChannel(index) => {
            if index < state.channels.len() {
                state.selected_channel = Some(index);
            }
        }
        Action::AddFound => {
            let Some(channel) = state.selected_channel.and_then(|i| state.channels.get(i)) else {
                state.message = "Select a search result first".into();
                return vec![];
            };
            let value = json!({"id":channel.id,"name":channel.name});
            if state.append("channel_whitelist", value) {
                return close(state);
            }
        }
        Action::AddManual => {
            let id = state.channel_id.text.trim();
            if id.is_empty() {
                state.message = "Channel ID cannot be empty".into();
                return vec![];
            }
            let name = if state.channel_name.text.trim().is_empty() {
                id
            } else {
                &state.channel_name.text
            };
            let value = json!({"id":id,"name":name});
            if state.append("channel_whitelist", value) {
                return close(state);
            }
        }
    }
    vec![]
}
