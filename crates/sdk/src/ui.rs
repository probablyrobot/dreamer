//! Building a view tree: layout nodes (`column`, `row`, `split`, `tabs`,
//! `scroll`) holding data nodes (`table`, `list`, `log`, `text`, `gauge`,
//! `chart`, `form`). Every node has a key, unique among its siblings; the
//! host compares a new tree with the last one key by key, so keep a
//! node's key the same while it is the same thing (a row's id, not its
//! place).
//!
//! ```
//! use dreamer_sdk::ui;
//!
//! let screen = ui::screen(
//!     "Panes",
//!     ui::split("main", [
//!         ui::table("panes", ["id", "command"])
//!             .title("Panes")
//!             .row("1", ["%1", "zsh"])
//!             .row("2", ["%2", "vim"])
//!             .selected("2"),
//!         ui::column("side", [
//!             ui::gauge("load", 0.4, "40%").title("Load"),
//!             ui::chart("history", [1.0, 3.0, 2.0]),
//!         ]),
//!     ])
//!     .sizes([60, 40]),
//! )
//! .status("q quit")
//! .focus("panes");
//! let frame: view_tree::Frame = screen.into();
//! assert_eq!(frame.root.children.len(), 2);
//! ```

use view_tree::{Bind, Direction, Field, Frame, Kind, Node};

/// A node being built. Methods that do not apply to its kind (`row` on a
/// text node, say) change nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct El(Node);

impl El {
    fn new(key: impl Into<String>, kind: Kind) -> El {
        El(Node::new(key, kind))
    }

    fn with(key: impl Into<String>, kind: Kind, children: impl IntoIterator<Item = El>) -> El {
        let mut n = Node::new(key, kind);
        n.children = children.into_iter().map(|c| c.0).collect();
        El(n)
    }

    /// A heading: a border with it in a terminal, and a tab's name.
    pub fn title(mut self, title: impl Into<String>) -> El {
        self.0.title = Some(title.into());
        self
    }

    /// Add a child to a layout node.
    pub fn child(mut self, child: El) -> El {
        if self.0.kind.is_layout() {
            self.0.children.push(child.0);
        }
        self
    }

    /// A split's weights, one per child.
    pub fn sizes(mut self, weights: impl IntoIterator<Item = u16>) -> El {
        if let Kind::Split { sizes, .. } = &mut self.0.kind {
            *sizes = weights.into_iter().collect();
        }
        self
    }

    /// A split one above the other, rather than side by side.
    pub fn vertical(mut self) -> El {
        if let Kind::Split { direction, .. } = &mut self.0.kind {
            *direction = Direction::Column;
        }
        self
    }

    /// A table's row: its key and one cell per column.
    pub fn row<S: Into<String>>(
        mut self,
        key: impl Into<String>,
        cells: impl IntoIterator<Item = S>,
    ) -> El {
        if matches!(self.0.kind, Kind::Table { .. }) {
            self.0.children.push(item(key, cells));
        }
        self
    }

    /// A list's item, or a log's line: its key and its text.
    pub fn item(mut self, key: impl Into<String>, text: impl Into<String>) -> El {
        if matches!(self.0.kind, Kind::List { .. } | Kind::Log) {
            self.0.children.push(item(key, [text.into()]));
        }
        self
    }

    /// The selected row or item of a table or list, by key.
    pub fn selected(mut self, key: impl Into<String>) -> El {
        if let Kind::Table { selected, .. } | Kind::List { selected } = &mut self.0.kind {
            *selected = Some(key.into());
        }
        self
    }

    /// The node built.
    pub fn node(self) -> Node {
        self.0
    }
}

impl From<El> for Node {
    fn from(e: El) -> Node {
        e.0
    }
}

fn item<S: Into<String>>(key: impl Into<String>, cells: impl IntoIterator<Item = S>) -> Node {
    Node::new(
        key,
        Kind::Item {
            cells: cells.into_iter().map(Into::into).collect(),
        },
    )
}

/// Children one above the other.
pub fn column(key: impl Into<String>, children: impl IntoIterator<Item = El>) -> El {
    El::with(key, Kind::Column, children)
}

/// Children side by side.
pub fn row(key: impl Into<String>, children: impl IntoIterator<Item = El>) -> El {
    El::with(key, Kind::Row, children)
}

/// Children sharing the space by weight ([`El::sizes`]; equal without),
/// side by side or ([`El::vertical`]) one above the other.
pub fn split(key: impl Into<String>, children: impl IntoIterator<Item = El>) -> El {
    El::with(
        key,
        Kind::Split {
            direction: Direction::Row,
            sizes: Vec::new(),
        },
        children,
    )
}

/// One child shown at a time, the one at `active`; the children's titles
/// name the tabs.
pub fn tabs(key: impl Into<String>, active: usize, children: impl IntoIterator<Item = El>) -> El {
    El::with(key, Kind::Tabs { active }, children)
}

/// Children at their natural height, scrolled to keep the focus in view.
pub fn scroll(key: impl Into<String>, children: impl IntoIterator<Item = El>) -> El {
    El::with(key, Kind::Scroll, children)
}

/// A table with these columns; add rows with [`El::row`].
pub fn table<S: Into<String>>(key: impl Into<String>, columns: impl IntoIterator<Item = S>) -> El {
    El::new(
        key,
        Kind::Table {
            columns: columns.into_iter().map(Into::into).collect(),
            selected: None,
        },
    )
}

/// A list; add items with [`El::item`].
pub fn list(key: impl Into<String>) -> El {
    El::new(key, Kind::List { selected: None })
}

/// Lines that follow the newest; add them with [`El::item`].
pub fn log(key: impl Into<String>) -> El {
    El::new(key, Kind::Log)
}

/// The newest lines of pane `pane`, as the host follows them from a tap:
/// the lines never pass through the app.
pub fn tapped_log(key: impl Into<String>, pane: u32) -> El {
    El(Node::new(key, Kind::Log).bound(Bind::Tap(pane)))
}

/// Agent session `session`'s conversation, drawn by the host from the
/// session's events: streaming markdown, thinking, tool and approval
/// cards, the plan, and usage. The events never pass through the app.
pub fn transcript(key: impl Into<String>, session: impl Into<String>) -> El {
    El(Node::new(
        key,
        Kind::Transcript {
            thinking: false,
            back: 0,
        },
    )
    .bound(Bind::Session(session.into())))
}

/// A prompt for agent session `session`: several lines of `value` (the
/// app keeps it, as for an input), with `attachments` shown by label.
/// The host shows whether a turn runs from the session itself.
pub fn composer<S: Into<String>>(
    key: impl Into<String>,
    session: impl Into<String>,
    value: impl Into<String>,
    attachments: impl IntoIterator<Item = S>,
) -> El {
    El(Node::new(
        key,
        Kind::Composer {
            label: String::new(),
            value: value.into(),
            placeholder: String::new(),
            attachments: attachments.into_iter().map(Into::into).collect(),
        },
    )
    .bound(Bind::Session(session.into())))
}

/// Text, over as many lines as it has.
pub fn text(key: impl Into<String>, text: impl Into<String>) -> El {
    El::new(key, Kind::Text { text: text.into() })
}

/// A bar filled to `ratio` (0 to 1), with a label.
pub fn gauge(key: impl Into<String>, ratio: f64, label: impl Into<String>) -> El {
    El::new(
        key,
        Kind::Gauge {
            ratio: ratio.clamp(0.0, 1.0),
            label: label.into(),
        },
    )
}

/// Values as a sparkline.
pub fn chart(key: impl Into<String>, values: impl IntoIterator<Item = f64>) -> El {
    El::new(
        key,
        Kind::Chart {
            values: values.into_iter().collect(),
        },
    )
}

/// A form's fields as shown, with `focus` the one being edited.
pub fn form(key: impl Into<String>, fields: impl IntoIterator<Item = Field>, focus: usize) -> El {
    El::new(
        key,
        Kind::Form {
            fields: fields.into_iter().collect(),
            focus,
        },
    )
}

/// A text field of a form.
pub fn field(label: impl Into<String>, value: impl Into<String>) -> Field {
    Field {
        label: label.into(),
        value: value.into(),
        toggle: false,
        required: false,
    }
}

/// A toggle field of a form.
pub fn toggle(label: impl Into<String>, on: bool) -> Field {
    Field {
        label: label.into(),
        value: on.to_string(),
        toggle: true,
        required: false,
    }
}

/// The whole view: a title, the tree, and a status line.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen(Frame);

/// A view titled `title` showing `root`.
pub fn screen(title: impl Into<String>, root: El) -> Screen {
    Screen(Frame {
        title: title.into(),
        status: String::new(),
        focus: None,
        root: root.0,
    })
}

impl Screen {
    /// The status line: keys, the last outcome.
    pub fn status(mut self, status: impl Into<String>) -> Screen {
        self.0.status = status.into();
        self
    }

    /// The key of the node that takes keys, drawn as focused.
    pub fn focus(mut self, key: impl Into<String>) -> Screen {
        self.0.focus = Some(key.into());
        self
    }
}

impl From<Screen> for Frame {
    fn from(s: Screen) -> Frame {
        s.0
    }
}
