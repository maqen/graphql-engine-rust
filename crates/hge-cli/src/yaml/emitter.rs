//! A port of go-yaml v3's emitter (`emitterc.go`), which is itself a port of libyaml.
//!
//! The Hasura CLI writes every metadata file with go-yaml v3 (`yaml.NewEncoder` with
//! `SetIndent(2)`), so this follows that state machine step by step: the same indentation
//! rules, scalar style selection and quoting. go-yaml v3 sets no line width, so long
//! scalars are never folded. Comments, anchors
//! and aliases are never produced by the CLI and are left out.

/// The scalar styles of the emitter (`yaml_scalar_style_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarStyle {
    Any,
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Clone, Debug)]
pub enum Event {
    StreamStart,
    StreamEnd,
    DocumentStart {
        implicit: bool,
    },
    DocumentEnd {
        implicit: bool,
    },
    Scalar {
        tag: String,
        value: String,
        implicit: bool,
        quoted_implicit: bool,
        style: ScalarStyle,
    },
    SequenceStart {
        tag: String,
        implicit: bool,
        flow: bool,
    },
    SequenceEnd,
    MappingStart {
        tag: String,
        implicit: bool,
        flow: bool,
    },
    MappingEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    StreamStart,
    FirstDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    FlowSequenceFirstItem,
    FlowSequenceItem,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingSimpleValue,
    FlowMappingValue,
    BlockSequenceFirstItem,
    BlockSequenceItem,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingSimpleValue,
    BlockMappingValue,
    End,
}

#[derive(Default)]
struct ScalarData {
    value: Vec<u8>,
    multiline: bool,
    flow_plain_allowed: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
    style: Option<ScalarStyle>,
}

pub struct Emitter {
    out: Vec<u8>,
    best_indent: i64,
    best_width: i64,
    state: State,
    states: Vec<State>,
    indents: Vec<i64>,
    indent: i64,
    flow_level: i64,
    root_context: bool,
    sequence_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    column: i64,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
    foot_indent: i64,
    tag_handle: Vec<u8>,
    tag_suffix: Vec<u8>,
    scalar: ScalarData,
}

const TAG_PREFIX: &str = "tag:yaml.org,2002:";

/// Emit a complete event stream and return the text.
pub fn emit(events: &[Event], indent: i64) -> String {
    let mut e = Emitter::new(indent);
    for i in 0..events.len() {
        e.analyze_event(&events[i]);
        e.state_machine(events, i);
    }
    String::from_utf8(e.out).unwrap_or_default()
}

impl Emitter {
    fn new(indent: i64) -> Self {
        Emitter {
            out: Vec::new(),
            best_indent: indent,
            best_width: -1,
            state: State::StreamStart,
            states: Vec::new(),
            indents: Vec::new(),
            indent: -1,
            flow_level: 0,
            root_context: false,
            sequence_context: false,
            mapping_context: false,
            simple_key_context: false,
            column: 0,
            whitespace: true,
            indention: true,
            open_ended: false,
            foot_indent: -1,
            tag_handle: Vec::new(),
            tag_suffix: Vec::new(),
            scalar: ScalarData::default(),
        }
    }

    // --- low level writers (put / write / put_break / write_break)

    fn put(&mut self, b: u8) {
        self.out.push(b);
        self.column += 1;
    }

    fn put_break(&mut self) {
        self.out.push(b'\n');
        self.column = 0;
        self.indention = true;
    }

    /// Write the (possibly multi-byte) character at `*i` and advance.
    fn write(&mut self, s: &[u8], i: &mut usize) {
        let w = width(s[*i]).max(1);
        let end = (*i + w).min(s.len());
        self.out.extend_from_slice(&s[*i..end]);
        self.column += 1;
        *i = end;
    }

    fn write_all(&mut self, s: &[u8]) {
        let mut i = 0;
        while i < s.len() {
            self.write(s, &mut i);
        }
    }

    fn write_break(&mut self, s: &[u8], i: &mut usize) {
        if s[*i] == b'\n' {
            self.put_break();
            *i += 1;
        } else {
            self.write(s, i);
            self.column = 0;
            self.indention = true;
        }
    }

    // --- event analysis

    fn analyze_event(&mut self, event: &Event) {
        self.tag_handle.clear();
        self.tag_suffix.clear();
        self.scalar.value.clear();
        match event {
            Event::Scalar {
                tag,
                value,
                implicit,
                quoted_implicit,
                ..
            } => {
                if !tag.is_empty() && !*implicit && !*quoted_implicit {
                    self.analyze_tag(tag);
                }
                self.analyze_scalar(value.as_bytes());
            }
            Event::SequenceStart { tag, implicit, .. }
            | Event::MappingStart { tag, implicit, .. }
                if !tag.is_empty() && !*implicit =>
            {
                self.analyze_tag(tag);
            }
            _ => {}
        }
    }

    fn analyze_tag(&mut self, tag: &str) {
        // The default tag directives: "!" -> "!" and "!!" -> "tag:yaml.org,2002:".
        if let Some(rest) = tag.strip_prefix(TAG_PREFIX) {
            self.tag_handle = b"!!".to_vec();
            self.tag_suffix = rest.as_bytes().to_vec();
        } else if let Some(rest) = tag.strip_prefix('!') {
            self.tag_handle = b"!".to_vec();
            self.tag_suffix = rest.as_bytes().to_vec();
        } else {
            self.tag_suffix = tag.as_bytes().to_vec();
        }
    }

    fn analyze_scalar(&mut self, value: &[u8]) {
        let sd = &mut self.scalar;
        sd.value = value.to_vec();
        if value.is_empty() {
            sd.multiline = false;
            sd.flow_plain_allowed = false;
            sd.block_plain_allowed = true;
            sd.single_quoted_allowed = true;
            sd.block_allowed = false;
            return;
        }
        let mut block_indicators = false;
        let mut flow_indicators = false;
        let mut line_breaks = false;
        let mut special_characters = false;
        let mut tab_characters = false;
        let mut leading_space = false;
        let mut leading_break = false;
        let mut trailing_space = false;
        let mut trailing_break = false;
        let mut break_space = false;
        let mut space_break = false;
        let mut previous_space = false;
        let mut previous_break = false;

        if value.len() >= 3 && (value.starts_with(b"---") || value.starts_with(b"...")) {
            block_indicators = true;
            flow_indicators = true;
        }

        let mut preceded_by_whitespace = true;
        let mut i = 0;
        while i < value.len() {
            let w = width(value[i]).max(1);
            let followed_by_whitespace = i + w >= value.len() || is_blank(value, i + w);
            if i == 0 {
                match value[i] {
                    b'#' | b',' | b'[' | b']' | b'{' | b'}' | b'&' | b'*' | b'!' | b'|' | b'>'
                    | b'\'' | b'"' | b'%' | b'@' | b'`' => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    b'?' | b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'-' if followed_by_whitespace => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    _ => {}
                }
            } else {
                match value[i] {
                    b',' | b'?' | b'[' | b']' | b'{' | b'}' => flow_indicators = true,
                    b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'#' if preceded_by_whitespace => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    _ => {}
                }
            }

            if value[i] == b'\t' {
                tab_characters = true;
            } else if !is_printable(value, i) {
                special_characters = true;
            }
            if is_space(value, i) {
                if i == 0 {
                    leading_space = true;
                }
                if i + w == value.len() {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if is_break(value, i) {
                line_breaks = true;
                if i == 0 {
                    leading_break = true;
                }
                if i + w == value.len() {
                    trailing_break = true;
                }
                if previous_space {
                    space_break = true;
                }
                previous_space = false;
                previous_break = true;
            } else {
                previous_space = false;
                previous_break = false;
            }
            preceded_by_whitespace = is_blankz(value, i);
            i += w;
        }

        sd.multiline = line_breaks;
        sd.flow_plain_allowed = true;
        sd.block_plain_allowed = true;
        sd.single_quoted_allowed = true;
        sd.block_allowed = true;
        if leading_space || leading_break || trailing_space || trailing_break {
            sd.flow_plain_allowed = false;
            sd.block_plain_allowed = false;
        }
        if trailing_space {
            sd.block_allowed = false;
        }
        if break_space {
            sd.flow_plain_allowed = false;
            sd.block_plain_allowed = false;
            sd.single_quoted_allowed = false;
        }
        if space_break || tab_characters || special_characters {
            sd.flow_plain_allowed = false;
            sd.block_plain_allowed = false;
            sd.single_quoted_allowed = false;
        }
        if space_break || special_characters {
            sd.block_allowed = false;
        }
        if line_breaks {
            sd.flow_plain_allowed = false;
            sd.block_plain_allowed = false;
        }
        if flow_indicators {
            sd.flow_plain_allowed = false;
        }
        if block_indicators {
            sd.block_plain_allowed = false;
        }
    }

    // --- state machine

    fn pop_state(&mut self) {
        self.state = self.states.pop().unwrap_or(State::End);
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().unwrap_or(-1);
    }

    fn state_machine(&mut self, events: &[Event], i: usize) {
        let event = &events[i];
        match self.state {
            State::StreamStart => self.emit_stream_start(),
            State::FirstDocumentStart | State::DocumentStart => self.emit_document_start(event),
            State::DocumentContent => self.emit_document_content(events, i),
            State::DocumentEnd => self.emit_document_end(event),
            State::FlowSequenceFirstItem => self.emit_flow_sequence_item(events, i, true),
            State::FlowSequenceItem => self.emit_flow_sequence_item(events, i, false),
            State::FlowMappingFirstKey => self.emit_flow_mapping_key(events, i, true),
            State::FlowMappingKey => self.emit_flow_mapping_key(events, i, false),
            State::FlowMappingSimpleValue => self.emit_flow_mapping_value(events, i, true),
            State::FlowMappingValue => self.emit_flow_mapping_value(events, i, false),
            State::BlockSequenceFirstItem => self.emit_block_sequence_item(events, i, true),
            State::BlockSequenceItem => self.emit_block_sequence_item(events, i, false),
            State::BlockMappingFirstKey => self.emit_block_mapping_key(events, i, true),
            State::BlockMappingKey => self.emit_block_mapping_key(events, i, false),
            State::BlockMappingSimpleValue => self.emit_block_mapping_value(events, i, true),
            State::BlockMappingValue => self.emit_block_mapping_value(events, i, false),
            State::End => {}
        }
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        if self.indent < 0 {
            self.indent = if flow { self.best_indent } else { 0 };
        } else if !indentless {
            if self.states.last() == Some(&State::BlockSequenceItem) {
                // The first indent inside a sequence just skips the "- " indicator.
                self.indent += 2;
            } else {
                self.indent =
                    self.best_indent * ((self.indent + self.best_indent) / self.best_indent);
            }
        }
    }

    fn emit_stream_start(&mut self) {
        if self.best_indent < 2 || self.best_indent > 9 {
            self.best_indent = 2;
        }
        if self.best_width >= 0 && self.best_width <= self.best_indent * 2 {
            self.best_width = 80;
        }
        if self.best_width < 0 {
            self.best_width = i64::from(i32::MAX);
        }
        self.indent = -1;
        self.column = 0;
        self.whitespace = true;
        self.indention = true;
        self.foot_indent = -1;
        self.state = State::FirstDocumentStart;
    }

    fn emit_document_start(&mut self, event: &Event) {
        match event {
            Event::DocumentStart { implicit } => {
                let implicit = *implicit && self.state == State::FirstDocumentStart;
                if !implicit {
                    self.write_indent();
                    self.write_indicator(b"---", true, false, false);
                    self.write_indent();
                }
                self.state = State::DocumentContent;
            }
            _ => {
                // STREAM-END
                if self.open_ended {
                    self.write_indicator(b"...", true, false, false);
                    self.write_indent();
                }
                self.state = State::End;
            }
        }
    }

    fn emit_document_content(&mut self, events: &[Event], i: usize) {
        self.states.push(State::DocumentEnd);
        self.emit_node(events, i, true, false, false, false);
    }

    fn emit_document_end(&mut self, event: &Event) {
        // [Go] Force document foot separation.
        self.foot_indent = 0;
        self.foot_indent = -1;
        self.write_indent();
        if let Event::DocumentEnd { implicit: false } = event {
            self.write_indicator(b"...", true, false, false);
            self.write_indent();
        }
        self.state = State::DocumentStart;
    }

    fn emit_flow_sequence_item(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            self.write_indicator(b"[", true, true, false);
            self.increase_indent(true, false);
            self.flow_level += 1;
        }
        if let Event::SequenceEnd = events[i] {
            self.flow_level -= 1;
            self.pop_indent();
            if self.column == 0 {
                self.write_indent();
            }
            self.write_indicator(b"]", false, false, false);
            self.pop_state();
            return;
        }
        if !first {
            self.write_indicator(b",", false, false, false);
        }
        if self.column == 0 {
            self.write_indent();
        }
        if self.column > self.best_width {
            self.write_indent();
        }
        self.states.push(State::FlowSequenceItem);
        self.emit_node(events, i, false, true, false, false);
    }

    fn emit_flow_mapping_key(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            self.write_indicator(b"{", true, true, false);
            self.increase_indent(true, false);
            self.flow_level += 1;
        }
        if let Event::MappingEnd = events[i] {
            self.flow_level -= 1;
            self.pop_indent();
            self.write_indicator(b"}", false, false, false);
            self.pop_state();
            return;
        }
        if !first {
            self.write_indicator(b",", false, false, false);
        }
        if self.column == 0 {
            self.write_indent();
        }
        if self.column > self.best_width {
            self.write_indent();
        }
        if self.check_simple_key(events, i) {
            self.states.push(State::FlowMappingSimpleValue);
            self.emit_node(events, i, false, false, true, true);
            return;
        }
        self.write_indicator(b"?", true, false, false);
        self.states.push(State::FlowMappingValue);
        self.emit_node(events, i, false, false, true, false);
    }

    fn emit_flow_mapping_value(&mut self, events: &[Event], i: usize, simple: bool) {
        if simple {
            self.write_indicator(b":", false, false, false);
        } else {
            if self.column > self.best_width {
                self.write_indent();
            }
            self.write_indicator(b":", true, false, false);
        }
        self.states.push(State::FlowMappingKey);
        self.emit_node(events, i, false, false, true, false);
    }

    fn emit_block_sequence_item(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            // compact_sequence_indent is off in the CLI's encoder.
            self.increase_indent(false, false);
        }
        if let Event::SequenceEnd = events[i] {
            self.pop_indent();
            self.pop_state();
            return;
        }
        self.write_indent();
        self.write_indicator(b"-", true, false, true);
        self.states.push(State::BlockSequenceItem);
        self.emit_node(events, i, false, true, false, false);
    }

    fn emit_block_mapping_key(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            self.increase_indent(false, false);
        }
        if let Event::MappingEnd = events[i] {
            self.pop_indent();
            self.pop_state();
            return;
        }
        self.write_indent();
        if self.check_simple_key(events, i) {
            self.states.push(State::BlockMappingSimpleValue);
            self.emit_node(events, i, false, false, true, true);
            return;
        }
        self.write_indicator(b"?", true, false, true);
        self.states.push(State::BlockMappingValue);
        self.emit_node(events, i, false, false, true, false);
    }

    fn emit_block_mapping_value(&mut self, events: &[Event], i: usize, simple: bool) {
        if simple {
            self.write_indicator(b":", false, false, false);
        } else {
            self.write_indent();
            self.write_indicator(b":", true, false, true);
        }
        self.states.push(State::BlockMappingKey);
        self.emit_node(events, i, false, false, true, false);
    }

    fn emit_node(
        &mut self,
        events: &[Event],
        i: usize,
        root: bool,
        sequence: bool,
        mapping: bool,
        simple_key: bool,
    ) {
        self.root_context = root;
        self.sequence_context = sequence;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match &events[i] {
            Event::Scalar {
                implicit,
                quoted_implicit,
                style,
                ..
            } => self.emit_scalar(*implicit, *quoted_implicit, *style),
            Event::SequenceStart { flow, .. } => {
                self.process_tag();
                let empty = matches!(events.get(i + 1), Some(Event::SequenceEnd));
                self.state = if self.flow_level > 0 || *flow || empty {
                    State::FlowSequenceFirstItem
                } else {
                    State::BlockSequenceFirstItem
                };
            }
            Event::MappingStart { flow, .. } => {
                self.process_tag();
                let empty = matches!(events.get(i + 1), Some(Event::MappingEnd));
                self.state = if self.flow_level > 0 || *flow || empty {
                    State::FlowMappingFirstKey
                } else {
                    State::BlockMappingFirstKey
                };
            }
            _ => {}
        }
    }

    fn emit_scalar(&mut self, implicit: bool, quoted_implicit: bool, style: ScalarStyle) {
        self.select_scalar_style(implicit, quoted_implicit, style);
        self.process_tag();
        self.increase_indent(true, false);
        self.process_scalar();
        self.pop_indent();
        self.pop_state();
    }

    fn check_simple_key(&self, events: &[Event], i: usize) -> bool {
        let tag_len = self.tag_handle.len() + self.tag_suffix.len();
        let length = match &events[i] {
            Event::Scalar { .. } => {
                if self.scalar.multiline {
                    return false;
                }
                tag_len + self.scalar.value.len()
            }
            Event::SequenceStart { .. } => {
                if !matches!(events.get(i + 1), Some(Event::SequenceEnd)) {
                    return false;
                }
                tag_len
            }
            Event::MappingStart { .. } => {
                if !matches!(events.get(i + 1), Some(Event::MappingEnd)) {
                    return false;
                }
                tag_len
            }
            _ => return false,
        };
        length <= 128
    }

    fn select_scalar_style(&mut self, implicit: bool, quoted_implicit: bool, style: ScalarStyle) {
        let no_tag = self.tag_handle.is_empty() && self.tag_suffix.is_empty();
        let mut style = if style == ScalarStyle::Any {
            ScalarStyle::Plain
        } else {
            style
        };
        if self.simple_key_context && self.scalar.multiline {
            style = ScalarStyle::DoubleQuoted;
        }
        if style == ScalarStyle::Plain {
            if (self.flow_level > 0 && !self.scalar.flow_plain_allowed)
                || (self.flow_level == 0 && !self.scalar.block_plain_allowed)
            {
                style = ScalarStyle::SingleQuoted;
            }
            if self.scalar.value.is_empty() && (self.flow_level > 0 || self.simple_key_context) {
                style = ScalarStyle::SingleQuoted;
            }
            if no_tag && !implicit {
                style = ScalarStyle::SingleQuoted;
            }
        }
        if style == ScalarStyle::SingleQuoted && !self.scalar.single_quoted_allowed {
            style = ScalarStyle::DoubleQuoted;
        }
        if (style == ScalarStyle::Literal || style == ScalarStyle::Folded)
            && (!self.scalar.block_allowed || self.flow_level > 0 || self.simple_key_context)
        {
            style = ScalarStyle::DoubleQuoted;
        }
        if no_tag && !quoted_implicit && style != ScalarStyle::Plain {
            self.tag_handle = b"!".to_vec();
        }
        self.scalar.style = Some(style);
    }

    fn process_tag(&mut self) {
        if self.tag_handle.is_empty() && self.tag_suffix.is_empty() {
            return;
        }
        if !self.tag_handle.is_empty() {
            let handle = self.tag_handle.clone();
            if !self.whitespace {
                self.put(b' ');
            }
            self.write_all(&handle);
            self.whitespace = false;
            self.indention = false;
            if !self.tag_suffix.is_empty() {
                let suffix = self.tag_suffix.clone();
                self.write_tag_content(&suffix, false);
            }
        } else {
            self.write_indicator(b"!<", true, false, false);
            let suffix = self.tag_suffix.clone();
            self.write_tag_content(&suffix, false);
            self.write_indicator(b">", false, false, false);
        }
    }

    fn write_tag_content(&mut self, value: &[u8], need_whitespace: bool) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        let mut i = 0;
        while i < value.len() {
            let c = value[i];
            let must_write = matches!(
                c,
                b';' | b'/'
                    | b'?'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b','
                    | b'_'
                    | b'.'
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b'['
                    | b']'
            ) || is_alpha(c);
            if must_write {
                self.write(value, &mut i);
            } else {
                let w = width(c).max(1);
                for _ in 0..w {
                    let octet = value[i];
                    i += 1;
                    self.put(b'%');
                    self.put(hex_digit(octet >> 4));
                    self.put(hex_digit(octet & 0x0f));
                }
            }
        }
        self.whitespace = false;
        self.indention = false;
    }

    fn process_scalar(&mut self) {
        let value = std::mem::take(&mut self.scalar.value);
        let allow_breaks = !self.simple_key_context;
        match self.scalar.style.unwrap_or(ScalarStyle::Plain) {
            ScalarStyle::Plain | ScalarStyle::Any => self.write_plain_scalar(&value, allow_breaks),
            ScalarStyle::SingleQuoted => self.write_single_quoted_scalar(&value, allow_breaks),
            ScalarStyle::DoubleQuoted => self.write_double_quoted_scalar(&value, allow_breaks),
            ScalarStyle::Literal => self.write_literal_scalar(&value),
            ScalarStyle::Folded => self.write_folded_scalar(&value),
        }
        self.scalar.value = value;
    }

    fn write_indent(&mut self) {
        let indent = self.indent.max(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_break();
        }
        if self.foot_indent == indent {
            self.put_break();
        }
        while self.column < indent {
            self.put(b' ');
        }
        self.whitespace = true;
        self.foot_indent = -1;
    }

    fn write_indicator(
        &mut self,
        indicator: &[u8],
        need_whitespace: bool,
        is_whitespace: bool,
        is_indention: bool,
    ) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        self.write_all(indicator);
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
        self.open_ended = false;
    }

    fn write_plain_scalar(&mut self, value: &[u8], allow_breaks: bool) {
        if !value.is_empty() && !self.whitespace {
            self.put(b' ');
        }
        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && !is_space(value, i + 1)
                {
                    self.write_indent();
                    i += 1;
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else if is_break(value, i) {
                if !breaks && value[i] == b'\n' {
                    self.put_break();
                }
                self.write_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut i);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        if !value.is_empty() {
            self.whitespace = false;
        }
        self.indention = false;
        if self.root_context {
            self.open_ended = true;
        }
    }

    fn write_single_quoted_scalar(&mut self, value: &[u8], allow_breaks: bool) {
        self.write_indicator(b"'", true, false, false);
        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len() - 1
                    && !is_space(value, i + 1)
                {
                    self.write_indent();
                    i += 1;
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else if is_break(value, i) {
                if !breaks && value[i] == b'\n' {
                    self.put_break();
                }
                self.write_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                if value[i] == b'\'' {
                    self.put(b'\'');
                }
                self.write(value, &mut i);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.write_indicator(b"'", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_double_quoted_scalar(&mut self, value: &[u8], allow_breaks: bool) {
        let mut spaces = false;
        self.write_indicator(b"\"", true, false, false);
        let mut i = 0;
        while i < value.len() {
            if !is_printable(value, i)
                || is_bom(value, i)
                || is_break(value, i)
                || value[i] == b'"'
                || value[i] == b'\\'
            {
                let mut octet = value[i];
                let (w, mut v): (usize, u32) = if octet & 0x80 == 0 {
                    (1, u32::from(octet & 0x7F))
                } else if octet & 0xE0 == 0xC0 {
                    (2, u32::from(octet & 0x1F))
                } else if octet & 0xF0 == 0xE0 {
                    (3, u32::from(octet & 0x0F))
                } else if octet & 0xF8 == 0xF0 {
                    (4, u32::from(octet & 0x07))
                } else {
                    (1, u32::from(octet))
                };
                for k in 1..w {
                    octet = value.get(i + k).copied().unwrap_or(0);
                    v = (v << 6) + (u32::from(octet) & 0x3F);
                }
                i += w;
                self.put(b'\\');
                match v {
                    0x00 => self.put(b'0'),
                    0x07 => self.put(b'a'),
                    0x08 => self.put(b'b'),
                    0x09 => self.put(b't'),
                    0x0A => self.put(b'n'),
                    0x0b => self.put(b'v'),
                    0x0c => self.put(b'f'),
                    0x0d => self.put(b'r'),
                    0x1b => self.put(b'e'),
                    0x22 => self.put(b'"'),
                    0x5c => self.put(b'\\'),
                    0x85 => self.put(b'N'),
                    0xA0 => self.put(b'_'),
                    0x2028 => self.put(b'L'),
                    0x2029 => self.put(b'P'),
                    _ => {
                        let digits = if v <= 0xFF {
                            self.put(b'x');
                            2
                        } else if v <= 0xFFFF {
                            self.put(b'u');
                            4
                        } else {
                            self.put(b'U');
                            8
                        };
                        for k in (0..digits).rev() {
                            self.put(hex_digit(((v >> (k * 4)) & 0x0F) as u8));
                        }
                    }
                }
                spaces = false;
            } else if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len() - 1
                {
                    self.write_indent();
                    if is_space(value, i + 1) {
                        self.put(b'\\');
                    }
                    i += 1;
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else {
                self.write(value, &mut i);
                spaces = false;
            }
        }
        self.write_indicator(b"\"", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_block_scalar_hints(&mut self, value: &[u8]) {
        if is_space(value, 0) || is_break(value, 0) {
            let hint = [b'0' + self.best_indent as u8];
            self.write_indicator(&hint, false, false, false);
        }
        self.open_ended = false;
        let mut chomp: Option<u8> = None;
        if value.is_empty() {
            chomp = Some(b'-');
        } else {
            let mut i = value.len() - 1;
            while i > 0 && value[i] & 0xC0 == 0x80 {
                i -= 1;
            }
            if !is_break(value, i) {
                chomp = Some(b'-');
            } else if i == 0 {
                chomp = Some(b'+');
                self.open_ended = true;
            } else {
                i -= 1;
                while i > 0 && value[i] & 0xC0 == 0x80 {
                    i -= 1;
                }
                if is_break(value, i) {
                    chomp = Some(b'+');
                    self.open_ended = true;
                }
            }
        }
        if let Some(c) = chomp {
            self.write_indicator(&[c], false, false, false);
        }
    }

    fn write_literal_scalar(&mut self, value: &[u8]) {
        self.write_indicator(b"|", true, false, false);
        self.write_block_scalar_hints(value);
        // process_line_comment_linebreak(linebreak = true) with no comment: a line break.
        self.put_break();
        self.whitespace = true;
        let mut breaks = true;
        let mut i = 0;
        while i < value.len() {
            if is_break(value, i) {
                self.write_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut i);
                self.indention = false;
                breaks = false;
            }
        }
    }

    fn write_folded_scalar(&mut self, value: &[u8]) {
        self.write_indicator(b">", true, false, false);
        self.write_block_scalar_hints(value);
        self.put_break();
        self.whitespace = true;
        let mut breaks = true;
        let mut leading_spaces = true;
        let mut i = 0;
        while i < value.len() {
            if is_break(value, i) {
                if !breaks && !leading_spaces && value[i] == b'\n' {
                    let mut k = 0;
                    while is_break(value, k) {
                        k += width(value[k]).max(1);
                    }
                    if !is_blankz(value, k) {
                        self.put_break();
                    }
                }
                self.write_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                    leading_spaces = is_blank(value, i);
                }
                if !breaks
                    && is_space(value, i)
                    && !is_space(value, i + 1)
                    && self.column > self.best_width
                {
                    self.write_indent();
                    i += 1;
                } else {
                    self.write(value, &mut i);
                }
                self.indention = false;
                breaks = false;
            }
        }
    }
}

// --- character classes (yamlprivateh.go). Out-of-range reads see a NUL byte, like the
// zero-padded buffers of libyaml.

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn hex_digit(d: u8) -> u8 {
    if d < 10 { d + b'0' } else { d + b'A' - 10 }
}

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

pub(crate) fn is_printable(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    let c1 = at(b, i + 1);
    let c2 = at(b, i + 2);
    c == 0x0A
        || (0x20..=0x7E).contains(&c)
        || (c == 0xC2 && c1 >= 0xA0)
        || (c > 0xC2 && c < 0xED)
        || (c == 0xED && c1 < 0xA0)
        || c == 0xEE
        || (c == 0xEF && !(c1 == 0xBB && c2 == 0xBF) && !(c1 == 0xBF && (c2 == 0xBE || c2 == 0xBF)))
}

fn is_bom(b: &[u8], _i: usize) -> bool {
    at(b, 0) == 0xEF && at(b, 1) == 0xBB && at(b, 2) == 0xBF
}

fn is_space(b: &[u8], i: usize) -> bool {
    at(b, i) == b' '
}

fn is_blank(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b' ' || c == b'\t'
}

pub(crate) fn is_break(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b'\r'
        || c == b'\n'
        || (c == 0xC2 && at(b, i + 1) == 0x85)
        || (c == 0xE2 && at(b, i + 1) == 0x80 && at(b, i + 2) == 0xA8)
        || (c == 0xE2 && at(b, i + 1) == 0x80 && at(b, i + 2) == 0xA9)
}

fn is_blankz(b: &[u8], i: usize) -> bool {
    is_blank(b, i) || is_break(b, i) || at(b, i) == 0
}

fn width(b: u8) -> usize {
    if b & 0x80 == 0x00 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else if b & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}
