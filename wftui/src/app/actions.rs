//! `Action` execution and the fetches it spawns.
//!
//! Split out of `app.rs` (#714). A continuation `impl App` block, so no
//! type, signature or call site changed.

use super::*;

impl App {
    pub(super) fn execute_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Notice(msg) => self.set_status(msg),
            Action::PopScreen => {
                self.pop_screen();
            }
            Action::Quit => self.should_quit = true,
            Action::OpenThreadList(mut node_id, mut title) => {
                let mut target_url = None;
                if let Some(tree) = self.tree()
                    && let Some(node) = tree.nodes.iter().find(|n| n.node_id == node_id)
                {
                    if node.node_type == "Category" {
                        if let Some(child) = tree.nodes.iter().find(|c| {
                            (c.parent_node_id == node.node_id || c.depth > node.depth)
                                && c.node_type == "Forum"
                        }) {
                            node_id = child.node_id;
                            title = child.title.clone();
                        }
                    } else if matches!(node.node_type.as_str(), "LinkForum" | "Page") {
                        target_url = node.view_url.clone();
                    }
                }
                if let Some(url) = target_url {
                    self.open_url(&url);
                } else {
                    self.open_list(node_id, title);
                }
            }
            Action::OpenLatestThreads => self.open_list(0, "Latest posts".to_string()),
            Action::OpenThread(thread) => self.open_thread(&thread),
            Action::OpenProfile(user_id, name) => self.open_profile(user_id, &name),
            Action::OpenMemberContent {
                user_id,
                username,
                content,
            } => {
                let display_title = format!("{username}'s {content}");
                let s = screens::SearchState {
                    query: format!("by: {username} ({content})"),
                    input_mode: false,
                    loading: true,
                    // The screen remembers it is a member's content list, so
                    // paging and `t` re-issue `search_member` instead of
                    // searching for that label (issue #548).
                    member: Some((user_id, content.clone())),
                    content_type: if content == "post" { 2 } else { 1 },
                    ..Default::default()
                };
                self.push_screen(Screen::Search(s));
                self.load_member_content(user_id, content, 1);
                self.set_hint(format!("Searching {display_title}…"));
            }
            Action::OpenConversation(conv) => self.open_conversation(conv),
            Action::LoadForum(node_id, page) => self.load_forum(node_id, page),
            Action::LoadThread(id, page) => self.load_thread(id, page),
            Action::LoadConversations(page) => self.load_conversations(page),
            // Paging inside a conversation the user already opened.
            Action::LoadConversation(id, page) => self.load_conversation(id, page, true),
            Action::LoadAlerts => self.load_alerts(),
            Action::LoadNodes => {
                if let Some(tree) = self.tree_mut() {
                    tree.loading = true;
                    tree.error = None;
                }
                self.load_nodes();
            }
            Action::RunSearchQuery(query) => self.run_search_query(query),
            Action::OpenMediaGallery => {
                self.push_screen(Screen::MediaGallery(screens::MediaListState {
                    page: 1,
                    loading: true,
                    category_title: "All media".into(),
                    ..Default::default()
                }));
                self.load_media(None, 1);
                self.load_media_categories();
            }
            Action::OpenResources => {
                self.push_screen(Screen::Resources(screens::ResourceListState {
                    page: 1,
                    loading: true,
                    ..Default::default()
                }));
                self.load_resources(1);
            }
            Action::LoadMedia { category, page } => self.load_media(category, page),
            Action::LoadResources(page) => self.load_resources(page),
            Action::LoadMediaCategories => self.load_media_categories(),
            Action::OpenResource(id) => {
                self.push_screen(Screen::ResourceView(screens::ResourceViewState {
                    id,
                    loading: true,
                    ..Default::default()
                }));
                self.load_resource(id);
            }
            Action::LoadResource(id) => self.load_resource(id),
            Action::PlayVideo { url, title } => {
                // #711 played this in a pane. Measured, that cost 7.2 Mbit/s
                // of terminal traffic on half-blocks and 26 Mbit/s on kitty
                // — for a 360p video the reader could have watched at about
                // 1 — with every byte of it flowing through the production
                // web server. The picture was not worth the pipe, so a video
                // opens where videos are cheap: the browser.
                let _ = title;
                self.open_url(&url);
            }
            Action::OpenImage(open) => {
                self.push_screen(Screen::ImageView(screens::ImageViewState::of(*open)));
            }
            Action::LoadMemberContent {
                user_id,
                content,
                page,
            } => self.load_member_content(user_id, content, page),
            Action::MarkThreadRead(id) => self.mark_thread_read(id, None),
            Action::MarkForumRead(node_id) => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api
                        .mark_forum_read(node_id)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::MarkedRead(result)).ok();
                });
            }
            Action::MarkAlertRead(id) => self.mark_alert_read(id),
            Action::MarkConversationRead(id) => self.mark_conversation_read(id),
            Action::ReactPost(post_id) => {
                // A like is an attributed server-side mutation like any other
                // write: it belongs to the session that started it and must
                // be aborted by `end_session` (issue #567's rule), not sent
                // under whatever token is live by the time it fires.
                let api = self.api.clone();
                let tx = self.tx.clone();
                self.spawn_write(async move {
                    let result = api.react_post(post_id, 1).await.map_err(|e| TaskError::of(&e));
                    tx.send(Msg::PostToggled { verb: PostVerb::Like, result }).ok();
                });
            }
            Action::VotePost(post_id, vote_type) => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                let verb = PostVerb::of_vote(&vote_type);
                self.spawn_write(async move {
                    let result = api
                        .vote_post(post_id, &vote_type)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::PostToggled { verb, result }).ok();
                });
            }
            Action::OpenDrafts => self.open_drafts(),
            Action::ResumeDraft(key) => self.resume_draft(key),
            Action::DropDraft(key) => {
                self.discard_draft(key);
                self.set_status("Draft deleted.");
                // Rebuild in place rather than popping: deleting one draft
                // should leave you looking at the rest.
                if let Some(Screen::Drafts(_)) = self.screens.last() {
                    let rows = self.draft_rows();
                    if let Some(Screen::Drafts(d)) = self.screens.last_mut() {
                        d.sel = d.sel.min(rows.len().saturating_sub(1));
                        d.rows = rows;
                    }
                }
            }
            Action::DiscardDraft => {
                if let Some(Screen::Compose(c)) = self.screens.last()
                    && let Some(key) = c.target.as_ref().map(|t| t.draft_key())
                {
                    self.discard_draft(key);
                    self.set_status("Draft discarded.");
                }
            }
            Action::StartReply(thread) => self.reply_to_thread(&thread),
            Action::StartEditPost(thread, post) => {
                // Seeded with what is there now, caret at the start: an edit
                // usually fixes the top of a post, not appends to it.
                self.push_screen(Screen::Compose(screens::ComposeState {
                    target: Some(ComposeTarget::EditPost {
                        post_id: post.post_id,
                        thread_id: thread.thread_id,
                        thread_title: thread.title.clone(),
                    }),
                    author: self.me_name(),
                    body: post.message.clone(),
                    ..Default::default()
                }));
            }
            Action::UploadAttachment { path, context } => {
                let api = self.client.clone();
                let tx = self.tx.clone();
                let key = self.compose_attachment_key();
                if let Some(Screen::Compose(c)) = self.screens.last_mut() {
                    c.uploading = true;
                    c.error = None;
                }
                self.spawn_write(async move {
                    let result = App::upload_from_path(&api, &path, context, key.as_deref()).await;
                    tx.send(Msg::AttachmentUploaded(result)).ok();
                });
            }
            Action::SubmitEdit { post_id, message } => {
                let api = self.api.clone();
                let key = self.compose_attachment_key();
                let tx = self.tx.clone();
                self.spawn_write(async move {
                    let result = api
                        .edit_post(post_id, &message, key.as_deref())
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::PostEdited { post_id, result }).ok();
                });
            }
            Action::DeletePost { post_id, thread_id } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                self.spawn_write(async move {
                    // Soft delete, always: it is what XF's own UI does and it
                    // leaves the post recoverable. A hard delete from a
                    // terminal keystroke is not a thing this client offers.
                    let result = api
                        .delete_post(post_id, false)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::PostDeleted { post_id, thread_id, result }).ok();
                });
            }
            Action::MarkSolution { post_id, thread_id } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                self.spawn_write(async move {
                    let result = api
                        .mark_solution(post_id)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::SolutionMarked { post_id, thread_id, result }).ok();
                });
            }
            Action::StartReplyQuoting(thread, post) => {
                // The quote block is built to WindowsForum's ContentIntegrity
                // contract (#707): `post:`/`member:` naming the real source
                // and its author, and the body stripped of nested quotes the
                // way XF's own "reply with quote" strips them. Anything else
                // is recorded as an altered quote on save.
                let quote = common::bbcode::quote_block(
                    &post.username,
                    post.post_id,
                    post.user_id,
                    &post.message,
                );
                self.reply_to_thread_with(&thread, quote);
            }
            Action::StartReplyConversation(conv) => self.reply_to_conversation(&conv),
            Action::StartNewThread(node_id) => self.new_thread(node_id),
            Action::StartNewConversation(recipient) => {
                let mut state = screens::NewConversationState::default();
                if let Some(r) = recipient {
                    state.recipients = format!("{r}, ");
                    // Char count, not byte length — editor.rs cursors are
                    // char indices, and a non-ASCII username (e.g. "Zoë")
                    // has more bytes than chars (issue #535).
                    state.recipients_cursor = state.recipients.chars().count();
                    state.field = 1;
                }
                self.push_screen(Screen::NewConversation(state));
            }
            Action::SubmitReply { thread_id, message } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                // Whatever this draft uploaded under: without the key the
                // files are attached to nothing (#709).
                let key = self.compose_attachment_key();
                self.spawn_write(async move {
                    let result = api
                        .reply(thread_id, &message, key.as_deref())
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::ReplySent(result)).ok();
                });
            }
            Action::SubmitThread { node_id, title, message } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                let key = self.compose_attachment_key();
                self.spawn_write(async move {
                    let result = api
                        .create_thread(node_id, &title, &message, key.as_deref())
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::ThreadCreated(result)).ok();
                });
            }
            Action::SubmitConvoReply { id, message } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                self.spawn_write(async move {
                    let result = api
                        .reply_conversation(id, &message)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::ConvoReplySent(result)).ok();
                });
            }
            Action::ResolveRecipients(names, _title, _body) => {
                // The NewConversation screen keeps title/body; we only resolve
                // names to ids and report back per name.
                for name in names {
                    let api = self.api.clone();
                    let tx = self.tx.clone();
                    let name_clone = name.clone();
                    tokio::spawn(async move {
                        let id = api
                            .find_user(&name_clone)
                            .await
                            .map(|u| u.map(|u| u.user_id))
                            .map_err(|e| TaskError::of(&e));
                        tx.send(Msg::RecipientResolved { name: name_clone, id }).ok();
                    });
                }
            }
            Action::LoginBegin => self.begin_login(),
            Action::PasteClipboard => {
                let mut text = self.clipboard.clone();
                if text.is_empty()
                    && let Some(sys) = common::osc::read_from_system_clipboard()
                {
                    text = sys;
                }
                if text.is_empty() {
                    self.set_status("Nothing copied yet — drag, double-click, or use system clipboard.");
                    return;
                }
                self.handle_paste(text);
            }
            Action::OscCopy(value) => {
                self.copy_text(&value);
                self.set_status("Copied to your clipboard (OSC 52 + system clipboard).");
            }
            Action::OpenUrl(url) => self.open_url(&url),
        }
    }

    // ---- the go-to palette and the `g` chord ----

    /// Read a file and upload it as an attachment (#709).
    ///
    /// The read is capped: a terminal client is not the place to push a
    /// 500 MB file at the forum, and XF would refuse it anyway — better to
    /// say so before spending the upload.
    pub(super) async fn upload_from_path(
        api: &common::api::WfApiClient,
        path: &str,
        context: screens::AttachContext,
        key: Option<&str>,
    ) -> Result<(String, common::models::Attachment), String> {
        use screens::AttachContext;
        let expanded = shellexpand_home(path);
        let meta = std::fs::metadata(&expanded)
            .map_err(|e| format!("{expanded}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{expanded} is not a file."));
        }
        if meta.len() > MAX_UPLOAD_BYTES {
            return Err(format!(
                "{expanded} is {} MB; the limit here is {} MB.",
                meta.len() / (1024 * 1024),
                MAX_UPLOAD_BYTES / (1024 * 1024)
            ));
        }
        let bytes = std::fs::read(&expanded)
            .map_err(|e| format!("{expanded}: {e}"))?;
        let filename = std::path::Path::new(&expanded)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload".to_string());
        let mime = mime_for(&filename);
        let ctx: Vec<(&str, String)> = match context {
            AttachContext::Thread(id) => vec![("context[thread_id]", id.to_string())],
            AttachContext::Node(id) => vec![("context[node_id]", id.to_string())],
            AttachContext::Post(id) => vec![("context[post_id]", id.to_string())],
        };
        api.upload_attachment("post", &ctx, filename, bytes, mime, key)
            .await
            .map_err(|e| TaskError::of(&e).message)
    }

    /// The attachment key of the composer on top, if it uploaded anything
    /// (#709).
    pub(super) fn compose_attachment_key(&self) -> Option<String> {
        self.screens.iter().rev().find_map(|s| match s {
            Screen::Compose(c) => c.attachment_key.clone(),
            _ => None,
        })
    }

    /// Mark what was actually read (#694).
    ///
    /// Called when a thread view leaves the stack: the thread is marked read
    /// up to the newest post that was on screen, never to "now" — XF's
    /// mark-read takes a date and refuses to move the marker backwards, so a
    /// half-read thread stays half unread exactly as it would on the site.
    /// Nothing is sent when nothing new was seen, so backing in and out of a
    /// thread does not spend a request each time.
    pub(super) fn report_read(&mut self, view: &screens::ThreadViewState) {
        let seen = view.seen_date;
        if seen <= 0 || seen <= view.reported_date {
            return;
        }
        let id = view.thread.thread_id;
        // The client's own lists must agree without a refetch, the way the
        // conversation list already flips its row (#694).
        let fully_read = view.page >= view.last_page.max(1)
            && view.posts.iter().all(|p| p.post_date <= seen);
        if fully_read {
            for screen in self.screens.iter_mut() {
                let list = match screen {
                    Screen::Home(h) => &mut h.list,
                    Screen::ThreadList(l) => l,
                    _ => continue,
                };
                for t in list.threads.iter_mut() {
                    if t.thread_id == id {
                        t.is_unread = false;
                    }
                }
            }
        }
        self.mark_thread_read(id, Some(seen));
    }

    /// Infinite scroll (#700): keep the list ahead of the reader.
    ///
    /// Distinct from the automatic viewport fill in `Msg::ForumLoaded`: this
    /// one only fires once the selection is within a screenful of the last
    /// loaded row, so it cannot run without the reader moving.
    ///
    /// The viewport fill (#699) tops a list up to what the pane can show;
    /// this is the same idea carried forward as they move — once the
    /// selection is within a screenful of the last loaded row, the next
    /// server page is fetched and appended. One page in flight at a time
    /// (`loading` is the interlock), so a fast scroll walks forward a page
    /// per reply instead of firing a burst at the gate.
    ///
    /// Called once per tick rather than from the key handlers, so the wheel,
    /// the keys, `G`, and a click all feed it through one path.
    pub(super) fn autoload_more(&mut self) {
        let Some(screen) = self.screens.last_mut() else { return };
        let list = match screen {
            Screen::Home(h) => &mut h.list,
            Screen::ThreadList(l) => l,
            _ => return,
        };
        if list.loading || list.error.is_some() || list.visible == 0 {
            return;
        }
        let rows = list.threads.len();
        let next = list.page.max(1) + list.pages_loaded.max(1);
        if next > list.last_page || list.fill_budget == 0 {
            return;
        }
        // A screenful of lead, so the rows are there before the reader
        // arrives rather than after.
        let lead = list.visible;
        if rows == 0 || list.sel + lead < rows {
            return;
        }
        let node_id = list.node_id;
        let seq = list.load_seq;
        list.loading = true;
        // No budget spent here (#705): this fires because the reader has
        // scrolled to within a screenful of the end, which is intent. The
        // budget bounds the *automatic* fill, which runs without anyone
        // asking and is the one that ran away.
        self.load_forum_page(node_id, next, true, seq);
    }

    pub fn load_conversations(&mut self, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.conversations(page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationsLoaded { page, result }).ok();
        });
    }

    pub fn load_alerts(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.alerts(1).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::AlertsLoaded(result)).ok();
        });
    }

    pub fn mark_conversation_read(&mut self, id: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .mark_conversation_read(id)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationMarked(id, result)).ok();
        });
    }

    /// A fresh load: mints a new generation, so every reply still in flight
    /// for this list is now stale and will be dropped (#705).
    pub fn load_forum(&mut self, node_id: u32, page: u32) {
        let seq = self.next_list_seq;
        self.next_list_seq += 1;
        if let Some(list) = self.list_mut_for(node_id) {
            list.load_seq = seq;
            // A fresh load starts the window over: leaving `pages_loaded`
            // where the previous forum left it made the first fill ask for
            // `page + pages_loaded` — page 4 of a forum just opened (#705).
            list.pages_loaded = 1;
            list.fill_budget = FILL_PAGE_BUDGET;
        }
        self.load_forum_page(node_id, page, false, seq);
    }

    /// `append` marks a viewport-fill page (#699) — see `Msg::ForumLoaded`.
    /// `seq` is the generation the caller is filling for, never a new one.
    pub fn load_forum_page(&mut self, node_id: u32, page: u32, append: bool, seq: u64) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = if node_id == 0 {
                api.threads(page)
                    .await
                    .map(|r| ForumReply {
                        forum: Forum {
                            node_id: 0,
                            title: "Latest posts".to_string(),
                            ..Default::default()
                        },
                        threads: r.threads,
                        pagination: r.pagination,
                        sticky: Vec::new(),
                    })
                    .map_err(|e| TaskError::of(&e))
            } else {
                api.forum(node_id, page).await.map_err(|e| TaskError::of(&e))
            };
            tx.send(Msg::ForumLoaded { node_id, page, append, seq, result }).ok();
        });
    }

    // ---- where forum/thread-list state lives ----
    //
    // Home owns both a tree and a list; the pre-redesign ForumTree/ThreadList
    // screens still exist for pushes from search, alerts and the narrow
    // layout. These three helpers are the single place that knows both shapes,
    // so a message handler never has to.

    pub fn load_thread(&mut self, id: u32, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.thread(id, page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ThreadLoaded { id, page, result }).ok();
        });
    }

    pub fn mark_thread_read(&mut self, id: u32, date: Option<i64>) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.mark_thread_read(id, date).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::MarkedRead(result)).ok();
        });
    }

    pub fn mark_alert_read(&mut self, id: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.mark_alert_read(id).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::AlertMarked(result)).ok();
        });
    }

    pub fn load_nodes(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.nodes().await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::NodesLoaded(result)).ok();
        });
    }

    /// `mark_read` must be `true` only for a load the user asked for (opening
    /// a conversation, paging inside it, the post-reply reload) — never for
    /// the dual-pane Inbox priming its view pane (issue #541).
    pub fn load_conversation(&mut self, id: u32, page: u32, mark_read: bool) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .conversation(id, page)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationLoaded { id, page, mark_read, result }).ok();
        });
    }

    /// One page of a member's threads or posts (issue #548). Member content
    /// comes from `/search/member`, which takes the user id and `content`
    /// directly — the Search screen's `query` there is a display label, not a
    /// search term, so it must never be sent as one.
    pub fn load_member_content(&mut self, user_id: u32, content: String, page: u32) {
        self.set_hint("Searching…");
        let generation = self.begin_search_load();
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .search_member(user_id, &content, page)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::SearchDone { generation, page, result }).ok();
        });
    }

    pub fn run_search_query(&mut self, query: SearchQuery) {
        let generation = self.begin_search_load();
        let api = self.api.clone();
        let tx = self.tx.clone();
        let page = query.page;
        self.set_hint("Searching…");
        tokio::spawn(async move {
            let result = api
                .search_advanced(&query)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::SearchDone { generation, page, result }).ok();
        });
    }

    /// Fetch one page of the XFMG media catalog, whole or by category
    /// (issues #680, #697).
    pub fn load_media(&mut self, category: Option<u32>, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .media_list(category, page)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::MediaLoaded { page, result }).ok();
        });
    }

    /// The gallery's category tree (#697).
    pub fn load_media_categories(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.media_categories().await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::MediaCategoriesLoaded(result)).ok();
        });
    }

    /// One resource, for the in-client page (#697).
    pub fn load_resource(&mut self, id: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.resource(id).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ResourceViewLoaded { id, result }).ok();
        });
    }

    /// Fetch one page of the XFRM resource catalog (issue #680).
    pub fn load_resources(&mut self, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.resources_list(page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ResourceLoaded { page, result }).ok();
        });
    }

    /// Mint the next search-load identity and mark the topmost Search
    /// screen as the one waiting on it. Every search load originates from a
    /// Search screen (it is pushed before its first fetch), so the screen
    /// arm always matches in practice; if it somehow does not, the minted
    /// number is stamped on no screen and the reply can never be adopted.
    pub(super) fn begin_search_load(&mut self) -> u64 {
        self.search_generation += 1;
        let generation = self.search_generation;
        if let Some(search) = self.screens.iter_mut().rev().find_map(|s| match s {
            Screen::Search(search) => Some(search),
            _ => None,
        }) {
            search.generation = generation;
        }
        generation
    }

    /// Load one image off the UI thread: disk cache first, then the site
    /// through `api_gate`, then decode + encode on a blocking task. Only the
    /// finished payload crosses back, as `Msg::ImageLoaded`.
    #[cfg(feature = "images")]
    pub(super) fn spawn_image_load(&mut self, pending: crate::images::Pending) {
        let Some(picker) = self.images.picker() else {
            return;
        };
        let client = self.client.clone();
        let disk = self.images.disk();
        let tx = self.tx.clone();
        let slots = self.image_slots.clone();
        tokio::spawn(async move {
            // Decoration waits its turn behind at most a couple of siblings;
            // `image_gate` then spaces the ones that get through, in a lane of
            // its own so nothing here delays an interactive call (issue #543).
            let _permit = slots.acquire_owned().await;
            let key = pending.store_key();
            let result = crate::images::load(&client, &disk, picker, &pending).await;
            tx.send(Msg::ImageLoaded { key, result }).ok();
        });
    }

    #[cfg(not(feature = "images"))]
    pub(super) fn spawn_image_load(&mut self, _pending: crate::images::Pending) {}

    pub fn open_url(&mut self, url: &str) {
        if url.is_empty() {
            return;
        }
        let target = match resolve_open_url(url, &common::config::base_url()) {
            Ok(target) => target,
            Err(scheme) => {
                self.set_status(format!("Refused to open \"{scheme}:\" link."));
                return;
            }
        };
        // Remote sessions: even if the opener targets the wrong machine, the
        // URL is now in the user's local clipboard.
        self.copy_text(&target);
        self.set_status(format!("Opening {target} (also copied to clipboard)"));
        let _ = common::oauth::open_browser(&target);
    }
}
