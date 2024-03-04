use std::{
    cmp::Reverse,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{mpsc::Receiver, Arc, OnceLock},
    time::{Duration, Instant},
};

use arboard::Clipboard;
use async_std::task::{sleep, spawn};
use bonsaidb::{
    core::schema::{Collection, SerializedCollection},
    local::Database,
};
use gpui::*;
use image::{DynamicImage, ImageBuffer};
use log::error;
use serde::{Deserialize, Serialize};
use time::{format_description, OffsetDateTime};

use crate::{
    commands::{RootCommand, RootCommandBuilder},
    components::{
        list::{AsyncListItems, Item, ListBuilder, ListItem},
        shared::{Icon, Img},
    },
    db::Db,
    paths::paths,
    query::TextInputWeak,
    state::{Action, ActionsModel, StateItem, StateModel, StateViewBuilder},
    swift,
    theme::Theme,
};

#[derive(Clone)]
pub struct ClipboardListBuilder {
    view: View<AsyncListItems>,
}

impl StateViewBuilder for ClipboardListBuilder {
    fn build(
        &self,
        query: &TextInputWeak,
        actions: &ActionsModel,
        update_receiver: Receiver<bool>,
        cx: &mut WindowContext,
    ) -> AnyView {
        query.set_placeholder("Search your clipboard history...", cx);

        actions.update_global(
            vec![Action::new(
                Img::list_icon(Icon::Trash, None),
                "Delete All",
                None,
                {
                    let view = self.view.clone();
                    move |actions, cx| {
                        if let Err(err) =
                            ClipboardListItem::prune(Duration::from_secs(0), view.downgrade(), cx)
                        {
                            error!("Failed to prune clipboard: {:?}", err);
                            actions
                                .toast
                                .error("Failed to delete clipboard entries", cx);
                        } else {
                            actions
                                .toast
                                .success("Successfully deleted clipboard entries", cx);
                        }
                    }
                },
                false,
            )],
            cx,
        );

        AsyncListItems::loader(&self.view, &actions, cx);
        let view = self.view.clone();
        ListBuilder::new()
            .build(
                query,
                &actions,
                move |_list, _, cx| {
                    let items = view.read(cx).items.clone();
                    let mut items: Vec<Item> = items.values().flatten().cloned().collect();
                    items.sort_by_key(|item| {
                        Reverse(
                            item.meta
                                .value()
                                .downcast_ref::<ClipboardListItem>()
                                .unwrap()
                                .copied_last,
                        )
                    });
                    return Ok(Some(items));
                },
                None,
                None,
                update_receiver,
                cx,
            )
            .into()
    }
}

#[derive(Clone, Serialize, Deserialize)]
enum ClipboardKind {
    Text {
        characters: u64,
        words: u64,
        text: String,
    },
    Image {
        width: u32,
        height: u32,
        thumbnail: PathBuf,
        path: PathBuf,
    },
}

#[derive(Clone, Serialize, Deserialize, Collection)]
#[collection(name = "clipboard.detail")]
struct ClipboardDetail {
    #[natural_id]
    id: u64,
    application: String,
    application_icon: Option<PathBuf>,
    kind: ClipboardKind,
}

#[derive(Clone, Serialize, Deserialize)]
enum ClipboardListItemKind {
    Text,
    Image { thumbnail: PathBuf },
}

impl Into<ClipboardListItemKind> for ClipboardKind {
    fn into(self) -> ClipboardListItemKind {
        match self {
            ClipboardKind::Text { .. } => ClipboardListItemKind::Text,
            ClipboardKind::Image { thumbnail, .. } => ClipboardListItemKind::Image { thumbnail },
        }
    }
}

impl Into<String> for ClipboardListItemKind {
    fn into(self) -> String {
        match self {
            ClipboardListItemKind::Text => "Text".to_string(),
            ClipboardListItemKind::Image { .. } => "Image".to_string(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Collection)]
#[collection(name = "clipboard.item")]
struct ClipboardListItem {
    #[natural_id]
    id: u64,
    title: String,
    #[serde(with = "time::serde::iso8601")]
    copied_first: OffsetDateTime,
    #[serde(with = "time::serde::iso8601")]
    copied_last: OffsetDateTime,
    kind: ClipboardListItemKind,
    copy_count: u32,
}

impl ClipboardListItem {
    fn new(id: u64, title: impl ToString, kind: ClipboardKind) -> Self {
        #[cfg(target_os = "macos")]
        let (application, icon_path) = {
            let app = unsafe { swift::get_frontmost_application_data() };
            if let Some(app) = app {
                let mut icon_path = paths().cache.clone();
                icon_path.push(format!("{}.png", app.id.to_string()));
                (app.name.to_string(), Some(icon_path))
            } else {
                ("Unknown".to_string(), None)
            }
        };
        #[cfg(not(target_os = "macos"))]
        let (application, icon_path) = ("Unknown".to_string(), None);

        let item = Self {
            id: id.clone(),
            title: title.to_string(),
            copied_last: OffsetDateTime::now_utc(),
            copied_first: OffsetDateTime::now_utc(),
            copy_count: 1,
            kind: kind.clone().into(),
        };
        let _ = item.clone().push_into(db_items());
        let detail = ClipboardDetail {
            id: id,
            application,
            application_icon: icon_path,
            kind,
        };
        let _ = detail.push_into(db_detail());

        item
    }
    fn get_item(&self, cx: &mut ViewContext<AsyncListItems>) -> Item {
        Item::new(
            self.id,
            vec![self.title.clone()],
            cx.new_view(|_| {
                ListItem::new(
                    match self.kind.clone() {
                        ClipboardListItemKind::Image { thumbnail } => {
                            Some(Img::list_file(thumbnail))
                        }
                        _ => Some(Img::list_icon(Icon::File, None)),
                    },
                    self.title.clone(),
                    None,
                    vec![],
                )
            })
            .into(),
            Some((
                0.66,
                Box::new({
                    let id = self.id.clone();
                    move |cx| StateItem::init(ClipboardPreview::init(id, cx), false, cx)
                }),
            )),
            vec![
                Action::new(
                    Img::list_icon(Icon::ClipboardPaste, None),
                    "Paste",
                    None,
                    {
                        let id = self.id.clone();
                        move |_, cx| {
                            let detail = ClipboardDetail::get(&id, db_detail()).unwrap().unwrap();
                            let _ = cx.update_window(cx.window_handle(), |_, cx| {
                                match detail.contents.kind.clone() {
                                    ClipboardKind::Text { text, .. } => {
                                        swift::close_and_paste(text.as_str(), false, cx);
                                    }
                                    ClipboardKind::Image { path, .. } => {
                                        swift::close_and_paste_file(&path, cx);
                                    }
                                    _ => {}
                                }
                            });
                        }
                    },
                    false,
                ),
                Action::new(
                    Img::list_icon(Icon::Trash, None),
                    "Delete",
                    None,
                    {
                        let self_clone = self.clone();
                        let view = cx.view().clone();
                        move |actions, cx| {
                            if let Err(err) = self_clone.delete(view.downgrade(), cx) {
                                error!("Failed to delete clipboard entry: {:?}", err);
                                actions.toast.error("Failed to delete clipboard entry", cx);
                            } else {
                                actions
                                    .toast
                                    .success("Successfully deleted clipboard entry", cx);
                            }
                        }
                    },
                    false,
                ),
            ],
            None,
            Some(Box::new(self.clone())),
            None,
        )
    }
    fn delete(&self, view: WeakView<AsyncListItems>, cx: &mut WindowContext) -> anyhow::Result<()> {
        view.update(cx, |view, cx| {
            view.remove(self.kind.clone().into(), self.id, cx);
        });

        if let Some(item) = ClipboardDetail::get(&self.id, db_detail())? {
            item.delete(db_detail())?;
        };
        if let Some(item) = Self::get(&self.id, db_items())? {
            item.delete(db_items())?;
        };
        match self.kind.clone() {
            ClipboardListItemKind::Image { thumbnail } => {
                let mut path = thumbnail.clone();
                path.pop();
                let _ = std::fs::remove_file(thumbnail);
                let _ = std::fs::remove_file(path.join(format!("{}.png", self.id)));
            }
            _ => {}
        }
        Ok(())
    }
    fn prune(
        age: Duration,
        view: WeakView<AsyncListItems>,
        cx: &mut WindowContext,
    ) -> anyhow::Result<()> {
        let items = Self::all(db_items()).query()?;
        for item in items {
            if item.contents.copied_last < OffsetDateTime::now_utc() - age {
                let _ = item.contents.delete(view.clone(), cx);
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ClipboardPreview {
    id: u64,
    item: ClipboardListItem,
    detail: ClipboardDetail,
    state: ListState,
}

impl ClipboardPreview {
    fn init(id: u64, cx: &mut WindowContext) -> Self {
        let item = ClipboardListItem::get(&id, db_items())
            .unwrap()
            .unwrap()
            .contents;
        let detail = ClipboardDetail::get(&id, db_detail())
            .unwrap()
            .unwrap()
            .contents;

        Self {
            id,
            item,
            detail: detail.clone(),
            state: ListState::new(
                1,
                ListAlignment::Top,
                Pixels(100.0),
                move |_, cx| match detail.kind.clone() {
                    ClipboardKind::Text { text, .. } => {
                        div().w_full().child(text.clone()).into_any_element()
                    }
                    ClipboardKind::Image {
                        width,
                        height,
                        path,
                        ..
                    } => div()
                        .size_full()
                        .child(
                            canvas(move |bounds, cx| {
                                img(ImageSource::File(Arc::new(path.clone())))
                                    .w(bounds.size.width)
                                    .h(Pixels(height as f32 / width as f32 * bounds.size.width.0))
                                    .into_any_element()
                                    .draw(
                                        bounds.origin,
                                        Size {
                                            width: AvailableSpace::MaxContent,
                                            height: AvailableSpace::MaxContent,
                                        },
                                        cx,
                                    );
                            })
                            .w_full(),
                        )
                        .into_any_element(),
                },
            ),
        }
    }
}

impl Render for ClipboardPreview {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let mut table = vec![
            (
                "Application".to_string(),
                div()
                    .flex()
                    .items_center()
                    .child(if let Some(icon) = self.detail.application_icon.clone() {
                        div().child(Img::list_file(icon)).mr_1()
                    } else {
                        div()
                    })
                    .child(self.detail.application.clone())
                    .into_any_element(),
            ),
            (
                "Last Copied".to_string(),
                self.item
                    .copied_last
                    .format(
                        &format_description::parse("[year]/[month]/[day] [hour]:[minute]:[second]")
                            .unwrap(),
                    )
                    .unwrap()
                    .into_any_element(),
            ),
            (
                "First Copied".to_string(),
                self.item
                    .copied_first
                    .format(
                        &format_description::parse("[year]/[month]/[day] [hour]:[minute]:[second]")
                            .unwrap(),
                    )
                    .unwrap()
                    .into_any_element(),
            ),
            (
                "Times Copied".to_string(),
                self.item.copy_count.to_string().into_any_element(),
            ),
            ("Content Type".to_string(), {
                let kind: String = self.item.kind.clone().into();
                kind.into_any_element()
            }),
        ];
        match self.detail.kind {
            ClipboardKind::Text {
                characters, words, ..
            } => {
                table.push((
                    "Characters".to_string(),
                    characters.to_string().into_any_element(),
                ));
                table.push(("Words".to_string(), words.to_string().into_any_element()));
            }
            ClipboardKind::Image { width, height, .. } => {
                table.push((
                    "Dimensions".to_string(),
                    format!("{}x{}", width, height).into_any_element(),
                ));
            }
        }
        div()
            .ml_2()
            .pl_2()
            .border_l_1()
            .border_color(theme.surface0)
            .h_full()
            .flex()
            .flex_col()
            .justify_between()
            .child(
                div()
                    .flex_1()
                    .p_2()
                    .text_xs()
                    .font(theme.font_mono.clone())
                    .child(list(self.state.clone()).size_full()),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.surface0)
                    .mt_auto()
                    .text_sm()
                    .p_2()
                    .children(
                        table
                            .into_iter()
                            .map(|(key, value)| {
                                div()
                                    .flex()
                                    .justify_between()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.subtext0)
                                            .child(key),
                                    )
                                    .child(value)
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
    }
}

impl StateViewBuilder for ClipboardPreview {
    fn build(
        &self,
        _query: &TextInputWeak,
        _actions: &ActionsModel,
        _update_receiver: Receiver<bool>,
        cx: &mut WindowContext,
    ) -> AnyView {
        cx.new_view(|_| self.clone()).into()
    }
}

pub(super) fn db_items() -> &'static Database {
    static DB: OnceLock<Database> = OnceLock::new();
    DB.get_or_init(|| Db::init_collection::<ClipboardListItem>())
}

pub(super) fn db_detail() -> &'static Database {
    static DB: OnceLock<Database> = OnceLock::new();
    DB.get_or_init(|| Db::init_collection::<ClipboardDetail>())
}

pub struct ClipboardCommandBuilder;

impl RootCommandBuilder for ClipboardCommandBuilder {
    fn build(&self, cx: &mut WindowContext) -> RootCommand {
        let view = cx.new_view(|cx| {
            let mut list_items = AsyncListItems::new();
            let items = ClipboardListItem::all(db_items())
                .query()
                .unwrap_or_default();
            for item in items {
                let item = item.clone().contents;
                list_items.push(item.kind.clone().into(), item.get_item(cx), cx);
            }
            cx.spawn(|view, mut cx| async move {
                let mut clipboard = Clipboard::new().unwrap();
                let mut hash: u64 = 0;
                let cache = paths().cache.join("clipboard");
                if !cache.exists() {
                    let _ = std::fs::create_dir_all(&cache);
                }
                let mut now = Instant::now();
                loop {
                    if Instant::now() - now > Duration::from_secs(3600) {
                        now = Instant::now();
                        // Prune clipboard history every hour, keeping entries for a week
                        let _ = cx.update_window(cx.window_handle(), |_, cx| {
                            let _ = ClipboardListItem::prune(
                                Duration::from_secs(60 * 60 * 24 * 7),
                                view.clone(),
                                cx,
                            );
                        });
                    }
                    if let Ok(text) = clipboard.get_text() {
                        let mut hasher = DefaultHasher::new();
                        text.hash(&mut hasher);
                        let new_hash = hasher.finish();
                        if new_hash != hash {
                            hash = new_hash;
                            let entry = if let Ok(Some(mut item)) =
                                ClipboardListItem::get(&hash, db_items())
                            {
                                item.contents.copied_last = OffsetDateTime::now_utc();
                                item.contents.copy_count += 1;
                                let _ = item.update(db_items());
                                item.contents.clone()
                            } else {
                                ClipboardListItem::new(
                                    hash.clone(),
                                    {
                                        let mut text = text.trim().replace("\n", " ");
                                        if text.len() > 25 {
                                            text.truncate(25);
                                            text.push_str("...");
                                        }
                                        text
                                    },
                                    ClipboardKind::Text {
                                        characters: text.chars().count() as u64,
                                        words: text.split_whitespace().count() as u64,
                                        text: text.clone(),
                                    },
                                )
                            };
                            let _ = cx.update_window(cx.window_handle(), |_, cx| {
                                let _ = view.update(cx, |view: &mut AsyncListItems, cx| {
                                    let item = entry.get_item(cx);
                                    view.push(entry.kind.into(), item, cx);
                                });
                            });
                        }
                    } else if let Ok(image) = clipboard.get_image() {
                        let mut hasher = DefaultHasher::new();
                        image.bytes.hash(&mut hasher);
                        let new_hash = hasher.finish();
                        if new_hash != hash {
                            hash = new_hash;
                            let entry = if let Ok(Some(mut item)) =
                                ClipboardListItem::get(&hash, db_items())
                            {
                                item.contents.copied_last = OffsetDateTime::now_utc();
                                item.contents.copy_count += 1;
                                let _ = item.update(db_items());
                                item.contents.clone()
                            } else {
                                let width = image.width.try_into().unwrap();
                                let height = image.height.try_into().unwrap();
                                let image = DynamicImage::ImageRgba8(
                                    ImageBuffer::from_vec(width, height, image.bytes.to_vec())
                                        .unwrap(),
                                );
                                let path = cache.join(format!("{}.png", hash));
                                let thumbnail = cache.join(format!("{}.thumb.png", hash));
                                let _ = image.save(&path);
                                let t = image.thumbnail(64, 64);
                                let _ = t.save(&thumbnail);
                                ClipboardListItem::new(
                                    hash.clone(),
                                    format!("Image ({}x{})", width, height),
                                    ClipboardKind::Image {
                                        width,
                                        height,
                                        path,
                                        thumbnail,
                                    },
                                )
                            };
                            let _ = cx.update_window(cx.window_handle(), |_, cx| {
                                let _ = view.update(cx, |view: &mut AsyncListItems, cx| {
                                    let item = entry.get_item(cx);
                                    view.push(entry.kind.into(), item, cx);
                                });
                            });
                        }
                    }
                    sleep(Duration::from_secs(1)).await;
                }
            })
            .detach();
            list_items
        });

        RootCommand::new(
            "clipboard",
            "Clipboard History",
            "Clipboard",
            Icon::Clipboard,
            Vec::<String>::new(),
            None,
            Box::new(move |_, cx| {
                let view = view.clone();
                StateModel::update(|this, cx| this.push(ClipboardListBuilder { view }, cx), cx);
            }),
        )
    }
}