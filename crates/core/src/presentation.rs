use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::TimelineEvent;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FriendlyFact {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresentedEvent {
    pub event_key: String,
    pub category_id: String,
    pub category_label: String,
    pub color: String,
    pub sequence: usize,
    pub start_offset_100ns: i64,
    pub end_offset_100ns: i64,
    pub seek_offset_ms: i64,
    pub raw_event_ids: Vec<i64>,
    pub details: Vec<FriendlyFact>,
    pub sensitive_event_id: Option<i64>,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventCategoryView {
    pub category_id: String,
    pub label: String,
    pub color: String,
    pub events: Vec<PresentedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresentedTimeline {
    pub session_id: String,
    pub total_events: usize,
    pub categories: Vec<EventCategoryView>,
}

pub fn group_presented_events(session_id: &str, events: Vec<PresentedEvent>) -> PresentedTimeline {
    const ORDER: &[&str] = &["pointer", "click", "drag", "scroll", "text", "system"];
    let total_events = events.len();
    let mut categories = Vec::new();
    for category_id in ORDER {
        let category_events = events
            .iter()
            .filter(|event| event.category_id == *category_id)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(first) = category_events.first() {
            categories.push(EventCategoryView {
                category_id: first.category_id.clone(),
                label: first.category_label.clone(),
                color: first.color.clone(),
                events: category_events,
            });
        }
    }
    PresentedTimeline {
        session_id: session_id.to_owned(),
        total_events,
        categories,
    }
}

struct PointerSegment {
    first_id: i64,
    last_id: i64,
    start_offset: i64,
    end_offset: i64,
    start_x: i64,
    start_y: i64,
    end_x: i64,
    end_y: i64,
    raw_ids: Vec<i64>,
}

struct ButtonPress {
    down_id: i64,
    start_offset: i64,
    button: String,
    start_x: i64,
    start_y: i64,
    end_x: i64,
    end_y: i64,
    moved: bool,
    raw_ids: Vec<i64>,
}

pub fn present_observed_events(events: &[TimelineEvent]) -> Vec<PresentedEvent> {
    let mut output = Vec::new();
    let mut pointer = None::<PointerSegment>;
    let mut button = None::<ButtonPress>;
    let requested_text = events
        .iter()
        .filter(|event| event.source == "requested_action" && event.kind == "type_text")
        .filter_map(|event| {
            event
                .tool_use_id
                .as_ref()
                .map(|tool_use_id| (tool_use_id.clone(), event.event_id))
        })
        .collect::<HashMap<_, _>>();
    for event in events {
        if event.source == "requested_action" {
            continue;
        }
        if event.source == "os_input" && event.kind == "pointer_move" {
            let Some((x, y)) = coordinates(event) else {
                continue;
            };
            if let Some(press) = button.as_mut() {
                press.end_x = x;
                press.end_y = y;
                press.moved |= press.start_x != x || press.start_y != y;
                press.raw_ids.push(event.event_id);
                continue;
            }
            match pointer.as_mut() {
                Some(segment) => {
                    segment.last_id = event.event_id;
                    segment.end_offset = event.offset_100ns;
                    segment.end_x = x;
                    segment.end_y = y;
                    segment.raw_ids.push(event.event_id);
                }
                None => {
                    pointer = Some(PointerSegment {
                        first_id: event.event_id,
                        last_id: event.event_id,
                        start_offset: event.offset_100ns,
                        end_offset: event.offset_100ns,
                        start_x: x,
                        start_y: y,
                        end_x: x,
                        end_y: y,
                        raw_ids: vec![event.event_id],
                    });
                }
            }
            continue;
        }
        flush_pointer(&mut pointer, &mut output);
        if event.source == "semantic_input" {
            let is_line = event.kind == "text_line";
            let start = event
                .public_payload
                .get("start_offset_100ns")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(event.offset_100ns);
            let command = event
                .public_payload
                .get("command")
                .and_then(serde_json::Value::as_str);
            let sensitive_event_id = if is_line {
                Some(event.event_id)
            } else {
                event
                    .tool_use_id
                    .as_ref()
                    .and_then(|tool_use_id| requested_text.get(tool_use_id).copied())
            };
            output.push(PresentedEvent {
                event_key: format!("event:{}", event.event_id),
                category_id: "text".into(),
                category_label: "Text input".into(),
                color: "#f4dc4e".into(),
                sequence: 0,
                start_offset_100ns: start,
                end_offset_100ns: event.offset_100ns,
                seek_offset_ms: event.offset_100ns / 10_000,
                raw_event_ids: vec![event.event_id],
                details: if is_line {
                    vec![FriendlyFact {
                        label: "Input".into(),
                        value: "Typed line".into(),
                    }]
                } else {
                    vec![FriendlyFact {
                        label: "Command".into(),
                        value: command.unwrap_or("Key command").to_owned(),
                    }]
                },
                sensitive_event_id,
                is_error: false,
            });
            continue;
        }
        if event.source == "os_input" && matches!(event.kind.as_str(), "key_down" | "key_up") {
            continue;
        }
        if event.source == "os_input" && event.kind == "button_down" {
            let (x, y) = coordinates(event).unwrap_or_default();
            button = Some(ButtonPress {
                down_id: event.event_id,
                start_offset: event.offset_100ns,
                button: button_name(event).into(),
                start_x: x,
                start_y: y,
                end_x: x,
                end_y: y,
                moved: false,
                raw_ids: vec![event.event_id],
            });
            continue;
        }
        if event.source == "os_input" && event.kind == "button_up" {
            let (x, y) = coordinates(event).unwrap_or_default();
            if let Some(mut press) = button.take() {
                press.end_x = x;
                press.end_y = y;
                press.moved |= press.start_x != x || press.start_y != y;
                press.raw_ids.push(event.event_id);
                output.push(present_button(press, event.offset_100ns));
            }
            continue;
        }
        if event.source == "os_input" && event.kind == "wheel" {
            let (x, y) = coordinates(event).unwrap_or_default();
            output.push(PresentedEvent {
                event_key: format!("event:{}", event.event_id),
                category_id: "scroll".into(),
                category_label: "Scroll".into(),
                color: "#a98bff".into(),
                sequence: 0,
                start_offset_100ns: event.offset_100ns,
                end_offset_100ns: event.offset_100ns,
                seek_offset_ms: event.offset_100ns / 10_000,
                raw_event_ids: vec![event.event_id],
                details: vec![
                    FriendlyFact {
                        label: "Position".into(),
                        value: format!("{x}, {y}"),
                    },
                    FriendlyFact {
                        label: "Direction".into(),
                        value: wheel_direction(event).into(),
                    },
                ],
                sensitive_event_id: None,
                is_error: false,
            });
            continue;
        }
        if event.source == "recorder" {
            output.push(PresentedEvent {
                event_key: format!("event:{}", event.event_id),
                category_id: "system".into(),
                category_label: "Recorder".into(),
                color: if event.kind.contains("error") {
                    "#ff6f75".into()
                } else {
                    "#8b9994".into()
                },
                sequence: 0,
                start_offset_100ns: event.offset_100ns,
                end_offset_100ns: event.offset_100ns,
                seek_offset_ms: event.offset_100ns / 10_000,
                raw_event_ids: vec![event.event_id],
                details: vec![FriendlyFact {
                    label: "Recorder".into(),
                    value: event.summary.clone(),
                }],
                sensitive_event_id: None,
                is_error: event.kind.contains("error"),
            });
        }
    }
    flush_pointer(&mut pointer, &mut output);
    if let Some(press) = button {
        output.push(present_button(
            press,
            events.last().map_or(0, |event| event.offset_100ns),
        ));
    }
    output.sort_by_key(|event| event.start_offset_100ns);
    let mut sequences = HashMap::<String, usize>::new();
    for event in &mut output {
        let sequence = sequences.entry(event.category_id.clone()).or_default();
        *sequence += 1;
        event.sequence = *sequence;
    }
    output
}

fn present_button(press: ButtonPress, end_offset: i64) -> PresentedEvent {
    let (category_id, category_label, color) = if press.moved {
        ("drag", "Drag", "#ff9b42")
    } else {
        ("click", "Click", "#ff6f75")
    };
    let details = if press.moved {
        vec![
            FriendlyFact {
                label: "Began at".into(),
                value: format!("{}, {}", press.start_x, press.start_y),
            },
            FriendlyFact {
                label: "Ended at".into(),
                value: format!("{}, {}", press.end_x, press.end_y),
            },
            FriendlyFact {
                label: "Button".into(),
                value: press.button,
            },
        ]
    } else {
        vec![
            FriendlyFact {
                label: "Position".into(),
                value: format!("{}, {}", press.end_x, press.end_y),
            },
            FriendlyFact {
                label: "Button".into(),
                value: press.button,
            },
        ]
    };
    PresentedEvent {
        event_key: format!(
            "button:{}-{}",
            press.down_id,
            press.raw_ids.last().copied().unwrap_or(press.down_id)
        ),
        category_id: category_id.into(),
        category_label: category_label.into(),
        color: color.into(),
        sequence: 0,
        start_offset_100ns: press.start_offset,
        end_offset_100ns: end_offset,
        seek_offset_ms: end_offset / 10_000,
        raw_event_ids: press.raw_ids,
        details,
        sensitive_event_id: None,
        is_error: false,
    }
}

fn flush_pointer(segment: &mut Option<PointerSegment>, output: &mut Vec<PresentedEvent>) {
    let Some(segment) = segment.take() else {
        return;
    };
    output.push(PresentedEvent {
        event_key: format!("pointer:{}-{}", segment.first_id, segment.last_id),
        category_id: "pointer".into(),
        category_label: "Pointer movement".into(),
        color: "#43d7e8".into(),
        sequence: 0,
        start_offset_100ns: segment.start_offset,
        end_offset_100ns: segment.end_offset,
        seek_offset_ms: segment.end_offset / 10_000,
        raw_event_ids: segment.raw_ids,
        details: vec![
            FriendlyFact {
                label: "Began at".into(),
                value: format!("{}, {}", segment.start_x, segment.start_y),
            },
            FriendlyFact {
                label: "Ended at".into(),
                value: format!("{}, {}", segment.end_x, segment.end_y),
            },
        ],
        sensitive_event_id: None,
        is_error: false,
    });
}

fn coordinates(event: &TimelineEvent) -> Option<(i64, i64)> {
    let details = event.public_payload.get("details")?;
    Some((details.get("x")?.as_i64()?, details.get("y")?.as_i64()?))
}

fn wheel_direction(event: &TimelineEvent) -> &'static str {
    let data = event
        .public_payload
        .pointer("/details/button_or_wheel_data")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let delta = ((data >> 16) as u16) as i16;
    if delta < 0 { "Down" } else { "Up" }
}

fn button_name(event: &TimelineEvent) -> &'static str {
    match event
        .public_payload
        .pointer("/details/message")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default()
    {
        513 | 514 => "Left",
        516 | 517 => "Right",
        519 | 520 => "Middle",
        _ => "Pointer",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::model::TimelineEvent;

    use super::present_observed_events;

    fn pointer(event_id: i64, offset_ms: i64, x: i64, y: i64) -> TimelineEvent {
        TimelineEvent {
            event_id,
            offset_100ns: offset_ms * 10_000,
            source: "os_input".into(),
            kind: "pointer_move".into(),
            summary: "raw pointer sample".into(),
            confidence: None,
            tool_use_id: None,
            public_payload: json!({ "details": { "x": x, "y": y, "button_state": 0 } }),
            has_encrypted_payload: false,
        }
    }

    fn button(
        event_id: i64,
        offset_ms: i64,
        kind: &str,
        message: i64,
        x: i64,
        y: i64,
    ) -> TimelineEvent {
        TimelineEvent {
            event_id,
            offset_100ns: offset_ms * 10_000,
            source: "os_input".into(),
            kind: kind.into(),
            summary: "raw button".into(),
            confidence: None,
            tool_use_id: None,
            public_payload: json!({ "details": { "x": x, "y": y, "message": message } }),
            has_encrypted_payload: false,
        }
    }

    #[test]
    fn pointer_samples_are_presented_as_begin_and_end_between_actions() {
        let mut events = vec![
            pointer(1, 100, 10, 20),
            pointer(2, 300, 30, 40),
            pointer(3, 2_000, 50, 60),
        ];
        events.push(TimelineEvent {
            event_id: 4,
            offset_100ns: 2_100 * 10_000,
            source: "os_input".into(),
            kind: "wheel".into(),
            summary: "raw wheel".into(),
            confidence: None,
            tool_use_id: None,
            public_payload: json!({ "details": { "x": 50, "y": 60, "button_or_wheel_data": 7_864_320 } }),
            has_encrypted_payload: false,
        });
        events.extend([pointer(5, 9_000, 70, 80), pointer(6, 12_000, 90, 100)]);

        let presented = present_observed_events(&events);
        let moves = presented
            .iter()
            .filter(|event| event.category_id == "pointer")
            .collect::<Vec<_>>();

        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].start_offset_100ns, 1_000_000);
        assert_eq!(moves[0].end_offset_100ns, 20_000_000);
        assert_eq!(moves[0].details[0].value, "10, 20");
        assert_eq!(moves[0].details[1].value, "50, 60");
        assert_eq!(moves[1].details[0].value, "70, 80");
        assert_eq!(moves[1].details[1].value, "90, 100");
    }

    #[test]
    fn button_pairs_become_clicks_or_drags_without_duplicate_pointer_events() {
        let events = vec![
            button(1, 100, "button_down", 513, 10, 20),
            button(2, 120, "button_up", 514, 10, 20),
            button(3, 500, "button_down", 513, 30, 40),
            pointer(4, 600, 50, 60),
            pointer(5, 700, 70, 80),
            button(6, 750, "button_up", 514, 70, 80),
        ];

        let presented = present_observed_events(&events);

        assert_eq!(presented.len(), 2);
        assert_eq!(presented[0].category_id, "click");
        assert_eq!(presented[1].category_id, "drag");
        assert_eq!(presented[1].details[1].value, "70, 80");
    }

    #[test]
    fn semantic_lines_are_visible_while_raw_keys_and_requests_stay_in_debug_only() {
        let events = vec![
            TimelineEvent {
                event_id: 1,
                offset_100ns: 100,
                source: "requested_action".into(),
                kind: "type_text".into(),
                summary: "requested".into(),
                confidence: Some(0.8),
                tool_use_id: Some("tool-1".into()),
                public_payload: json!({ "redacted": true }),
                has_encrypted_payload: true,
            },
            TimelineEvent {
                event_id: 2,
                offset_100ns: 200,
                source: "os_input".into(),
                kind: "key_down".into(),
                summary: "raw key".into(),
                confidence: None,
                tool_use_id: Some("tool-1".into()),
                public_payload: json!({}),
                has_encrypted_payload: true,
            },
            TimelineEvent {
                event_id: 3,
                offset_100ns: 400,
                source: "semantic_input".into(),
                kind: "text_line".into(),
                summary: "line".into(),
                confidence: None,
                tool_use_id: Some("tool-1".into()),
                public_payload: json!({ "start_offset_100ns": 250, "end_offset_100ns": 400, "text_length": 5 }),
                has_encrypted_payload: true,
            },
        ];

        let presented = present_observed_events(&events);

        assert_eq!(presented.len(), 1);
        assert_eq!(presented[0].category_id, "text");
        assert_eq!(presented[0].start_offset_100ns, 250);
        assert_eq!(presented[0].sensitive_event_id, Some(3));
    }
}
