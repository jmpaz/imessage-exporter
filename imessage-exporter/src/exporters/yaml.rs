use std::{
    collections::{
        HashMap,
        hash_map::Entry::{Occupied, Vacant},
    },
    fmt::Write as FmtWrite,
    fs::File,
    io::{BufWriter, Write},
};

use crate::{
    app::{error::RuntimeError, progress::ExportProgress, runtime::Config},
    exporters::exporter::{ATTACHMENT_NO_FILENAME, Exporter},
};

use imessage_database::{
    error::table::TableError,
    message_types::text_effects::TextEffect,
    message_types::variants::{CustomBalloon, Tapback, TapbackAction, Variant},
    tables::{
        attachment::Attachment,
        chat::Chat,
        messages::{
            Message,
            models::{AttachmentMeta, BubbleComponent, TextAttributes},
        },
        table::{ORPHANED, Table},
    },
    util::dates::format,
};

pub struct YAML<'a> {
    pub config: &'a Config,
    pub files: HashMap<String, BufWriter<File>>,
    pub orphaned: BufWriter<File>,
    pb: ExportProgress,
}

impl<'a> Exporter<'a> for YAML<'a> {
    fn new(config: &'a Config) -> Result<Self, RuntimeError> {
        let mut orphaned = config.options.export_path.clone();
        orphaned.push(ORPHANED);
        orphaned.set_extension("yaml");

        let file = File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&orphaned)?;
        let mut orphaned = BufWriter::new(file);
        write_yaml_header(&mut orphaned, config, None)?;

        Ok(Self {
            config,
            files: HashMap::new(),
            orphaned,
            pb: ExportProgress::new(),
        })
    }

    fn iter_messages(&mut self) -> Result<(), RuntimeError> {
        eprintln!(
            "Exporting to {} as yaml...",
            self.config.options.export_path.display()
        );

        let mut current_message_row = -1;

        let mut current_message = 0;
        let total_messages = Message::get_count(
            self.config.data_source.db(),
            &self.config.options.query_context,
        )?;
        self.pb.start(total_messages);

        let mut statement = Message::stream_rows(
            self.config.data_source.db(),
            &self.config.options.query_context,
        )?;

        let messages = statement
            .query_map([], |row| Ok(Message::from_row(row)))
            .map_err(|err| RuntimeError::DatabaseError(TableError::QueryError(err)))?;

        for message in messages {
            let mut msg = Message::extract(message)?;

            if msg.rowid == current_message_row {
                current_message += 1;
                continue;
            }
            current_message_row = msg.rowid;

            let _ = msg.generate_text(self.config.data_source.db());

            // Tapbacks/poll votes/updates are rendered in context (like TXT/HTML) to avoid duplication.
            if msg.is_tapback() || msg.is_poll_vote() || msg.is_poll_update() {
                current_message += 1;
                continue;
            }

            let yaml_item = self.format_message_yaml(&msg)?;
            YAML::write_to_file(self.get_or_create_file(&msg)?, &yaml_item)?;

            current_message += 1;
            if current_message % 99 == 0 {
                self.pb.set_position(current_message);
            }
        }

        self.pb.finish();
        Ok(())
    }

    fn get_or_create_file(
        &mut self,
        message: &Message,
    ) -> Result<&mut BufWriter<File>, RuntimeError> {
        match self.config.conversation(message) {
            Some((chatroom, _)) => {
                let filename = self.config.filename(chatroom);
                match self.files.entry(filename) {
                    Occupied(entry) => Ok(entry.into_mut()),
                    Vacant(entry) => {
                        let mut path = self.config.options.export_path.clone();
                        path.push(self.config.filename(chatroom));
                        path.set_extension("yaml");

                        let file = File::options()
                            .create(true)
                            .truncate(true)
                            .write(true)
                            .open(&path)?;
                        let mut writer = BufWriter::new(file);
                        write_yaml_header(&mut writer, self.config, Some(chatroom))?;

                        Ok(entry.insert(writer))
                    }
                }
            }
            None => Ok(&mut self.orphaned),
        }
    }

    fn write_to_file(file: &mut BufWriter<File>, text: &str) -> Result<(), RuntimeError> {
        file.write_all(text.as_bytes())
            .map_err(RuntimeError::DiskError)
    }
}

impl YAML<'_> {
    fn format_message_yaml(&self, message: &Message) -> Result<String, TableError> {
        let mut out = String::with_capacity(2048);

        // A YAML list item under `messages:`
        out.push_str("  - guid: ");
        out.push_str(&yaml_quote(&message.guid));
        out.push('\n');

        write_kv_i32(&mut out, 4, "rowid", message.rowid)?;
        write_kv_opt_i32(&mut out, 4, "chat_id", message.chat_id)?;
        write_kv_opt_i32(&mut out, 4, "deleted_from", message.deleted_from)?;
        write_kv_bool(&mut out, 4, "is_from_me", message.is_from_me())?;
        write_kv_bool(&mut out, 4, "is_read", message.is_read)?;

        let sender = self
            .config
            .who(message.handle_id, message.is_from_me(), &message.destination_caller_id);
        write_kv_str(&mut out, 4, "sender", sender)?;

        write_kv_opt_str(&mut out, 4, "service", message.service.as_deref())?;
        write_kv_opt_str(
            &mut out,
            4,
            "destination_caller_id",
            message.destination_caller_id.as_deref(),
        )?;
        write_kv_opt_str(&mut out, 4, "subject", message.subject.as_deref())?;

        // Variant/derived metadata
        self.write_variant(&mut out, message)?;

        // Timestamps
        out.push_str("    timestamps:\n");
        let sent = message.date(&self.config.offset);
        let sent_local = sent.as_ref().ok().map(|dt| dt.to_rfc3339());
        let sent_display = if message.date == 0 {
            None
        } else {
            Some(format(&sent))
        };
        write_timestamp_parts(
            &mut out,
            6,
            "sent",
            message.date,
            sent_local.as_deref(),
            sent_display.as_deref(),
        )?;

        let delivered = message.date_delivered(&self.config.offset);
        let delivered_local = delivered.as_ref().ok().map(|dt| dt.to_rfc3339());
        let delivered_display = if message.date_delivered == 0 {
            None
        } else {
            Some(format(&delivered))
        };
        write_timestamp_parts(
            &mut out,
            6,
            "delivered",
            message.date_delivered,
            delivered_local.as_deref(),
            delivered_display.as_deref(),
        )?;

        let read = message.date_read(&self.config.offset);
        let read_local = read.as_ref().ok().map(|dt| dt.to_rfc3339());
        let read_display = if message.date_read == 0 {
            None
        } else {
            Some(format(&read))
        };
        write_timestamp_parts(
            &mut out,
            6,
            "read",
            message.date_read,
            read_local.as_deref(),
            read_display.as_deref(),
        )?;

        let edited = message.date_edited(&self.config.offset);
        let edited_local = edited.as_ref().ok().map(|dt| dt.to_rfc3339());
        let edited_display = if message.date_edited == 0 {
            None
        } else {
            Some(format(&edited))
        };
        write_timestamp_parts(
            &mut out,
            6,
            "edited",
            message.date_edited,
            edited_local.as_deref(),
            edited_display.as_deref(),
        )?;
        write_kv_opt_str(
            &mut out,
            4,
            "time_until_read",
            message.time_until_read(&self.config.offset).as_deref(),
        )?;

        // Message text + translation
        if self.config.translated_messages.contains(&message.guid)
            && let Ok(Some(translation)) = message.get_translation(self.config.data_source.db())
        {
            out.push_str("    translation:\n");
            write_kv_str(&mut out, 6, "translated_text", &translation.translated_text)?;
            write_kv_str(&mut out, 6, "translation_lang", &translation.translation_lang)?;
            write_kv_str(&mut out, 6, "source_lang", &translation.source_lang)?;
        } else {
            out.push_str("    translation: null\n");
        }

        // Components + attachments (keeps bubble ordering)
        let mut attachments = Attachment::from_message(self.config.data_source.db(), message)?;
        out.push_str("    components:\n");
        self.write_components(&mut out, message, &mut attachments)?;

        // Flattened attachment list (useful for tooling/LLM ingestion)
        if attachments.is_empty() {
            out.push_str("    attachments: []\n");
        } else {
            out.push_str("    attachments:\n");
            for attachment in &attachments {
                out.push_str("      - rowid: ");
                let _ = write!(out, "{}", attachment.rowid);
                out.push('\n');
                write_kv_str(
                    &mut out,
                    8,
                    "path",
                    &self.config.message_attachment_path(attachment),
                )?;
            }
        }

        // Tapbacks in context
        self.write_tapbacks(&mut out, message)?;

        // Replies in context
        self.write_replies(&mut out, message)?;

        Ok(out)
    }

    fn write_variant(&self, out: &mut String, message: &Message) -> Result<(), TableError> {
        out.push_str("    variant:\n");

        match message.variant() {
            Variant::Normal => {
                if message.is_announcement() {
                    write_kv_str(out, 6, "kind", "announcement")?
                } else {
                    write_kv_str(out, 6, "kind", "normal")?
                }
            }
            Variant::Edited => write_kv_str(out, 6, "kind", "edited")?,
            Variant::SharePlay => write_kv_str(out, 6, "kind", "shareplay")?,
            Variant::App(balloon) => {
                write_kv_str(out, 6, "kind", "app")?;
                write_kv_opt_str(out, 6, "balloon_bundle_id", message.balloon_bundle_id.as_deref())?;
                write_kv_str(
                    out,
                    6,
                    "app_type",
                    match balloon {
                        CustomBalloon::URL => "url",
                        CustomBalloon::Handwriting => "handwriting",
                        CustomBalloon::DigitalTouch => "digital_touch",
                        CustomBalloon::ApplePay => "apple_pay",
                        CustomBalloon::Fitness => "fitness",
                        CustomBalloon::Slideshow => "slideshow",
                        CustomBalloon::FindMy => "find_my",
                        CustomBalloon::CheckIn => "check_in",
                        CustomBalloon::Polls => "polls",
                        CustomBalloon::Application(_) => "application",
                    },
                )?;
            }
            Variant::PollUpdate => {
                write_kv_str(out, 6, "kind", "poll_update")?;
                write_kv_opt_str(out, 6, "balloon_bundle_id", message.balloon_bundle_id.as_deref())?;
            }
            Variant::Vote => write_kv_str(out, 6, "kind", "poll_vote")?,
            Variant::Tapback(_, action, tapback) => {
                write_kv_str(out, 6, "kind", "tapback")?;
                write_kv_str(
                    out,
                    6,
                    "action",
                    match action {
                        TapbackAction::Added => "added",
                        TapbackAction::Removed => "removed",
                    },
                )?;

                match tapback {
                    Tapback::Loved => write_kv_str(out, 6, "tapback_type", "loved")?,
                    Tapback::Liked => write_kv_str(out, 6, "tapback_type", "liked")?,
                    Tapback::Disliked => write_kv_str(out, 6, "tapback_type", "disliked")?,
                    Tapback::Laughed => write_kv_str(out, 6, "tapback_type", "laughed")?,
                    Tapback::Emphasized => write_kv_str(out, 6, "tapback_type", "emphasized")?,
                    Tapback::Questioned => write_kv_str(out, 6, "tapback_type", "questioned")?,
                    Tapback::Sticker => write_kv_str(out, 6, "tapback_type", "sticker")?,
                    Tapback::Emoji(emoji) => {
                        write_kv_str(out, 6, "tapback_type", "emoji")?;
                        write_kv_opt_str(out, 6, "emoji", emoji)?;
                    }
                }
            }
            Variant::Unknown(code) => {
                write_kv_str(out, 6, "kind", "unknown")?;
                write_kv_i32(out, 6, "code", code)?;
            }
        }

        Ok(())
    }

    fn write_components(
        &self,
        out: &mut String,
        message: &Message,
        attachments: &mut Vec<Attachment>,
    ) -> Result<(), TableError> {
        let text_component_count = message
            .components
            .iter()
            .filter(|component| matches!(component, BubbleComponent::Text(_)))
            .count();

        let mut attachment_index: usize = 0;

        for component in &message.components {
            match component {
                BubbleComponent::Text(attrs) => {
                    out.push_str("      - kind: \"text\"\n");
                    self.write_text_attributes(
                        out,
                        message.text.as_deref(),
                        attrs,
                        text_component_count,
                    )?;
                }
                BubbleComponent::Attachment(meta) => {
                    out.push_str("      - kind: \"attachment\"\n");
                    let attachment = attachments.get_mut(attachment_index);
                    attachment_index += 1;

                    match attachment {
                        Some(attachment) => self.write_attachment_component(out, message, attachment, meta)?,
                        None => {
                            out.push_str("        attachment: null\n");
                            out.push_str("        meta: null\n");
                            write_kv_str(out, 8, "error", ATTACHMENT_NO_FILENAME)?;
                        }
                    }
                }
                BubbleComponent::App => {
                    out.push_str("      - kind: \"app\"\n");
                    write_kv_opt_str(out, 8, "balloon_bundle_id", message.balloon_bundle_id.as_deref())?;
                }
                BubbleComponent::Retracted => {
                    out.push_str("      - kind: \"retracted\"\n");
                }
            }
        }

        Ok(())
    }

    fn write_text_attributes(
        &self,
        out: &mut String,
        text: Option<&str>,
        attrs: &[TextAttributes],
        _text_component_count: usize,
    ) -> Result<(), TableError> {
        // YAML export represents message text in `components`; do not duplicate it at top-level.
        match text {
            Some(full_text) => {
                let segment = extract_text_segment(full_text, attrs);
                let component_text = if segment.is_empty() { full_text } else { &segment };
                write_kv_literal_str(out, 8, "text", component_text)?;
            }
            None => out.push_str("        text: null\n"),
        }

        // If there are no non-default effects, omit attributes entirely to reduce noise.
        let interesting_attrs: Vec<&TextAttributes> = attrs
            .iter()
            .filter(|attr| attr.effects.iter().any(|e| !matches!(e, TextEffect::Default)))
            .collect();

        if interesting_attrs.is_empty() {
            return Ok(());
        }

        out.push_str("        attributes:\n");
        for attr in interesting_attrs {
            out.push_str("          - start: ");
            let _ = write!(out, "{}", attr.start);
            out.push('\n');
            out.push_str("            end: ");
            let _ = write!(out, "{}", attr.end);
            out.push('\n');
            if let Some(text) = text
                && let Some(slice) = text.get(attr.start..attr.end)
            {
                write_kv_literal_str(out, 12, "slice", slice)?;
            } else {
                out.push_str("            slice: null\n");
            }

            out.push_str("            effects_debug:\n");
            for effect in &attr.effects {
                out.push_str("              - ");
                out.push_str(&yaml_quote(&format!("{effect:?}")));
                out.push('\n');
            }
        }

        Ok(())
    }

    fn write_attachment_component(
        &self,
        out: &mut String,
        message: &Message,
        attachment: &mut Attachment,
        meta: &AttachmentMeta,
    ) -> Result<(), TableError> {
        // Try to copy/convert the attachment so the export can reference a stable relative path.
        let _ = self
            .config
            .options
            .attachment_manager
            .handle_attachment(message, attachment, self.config);

        out.push_str("        attachment:\n");
        write_kv_i32(out, 10, "rowid", attachment.rowid)?;
        write_kv_opt_str(out, 10, "filename", attachment.filename.as_deref())?;
        write_kv_opt_str(out, 10, "transfer_name", attachment.transfer_name.as_deref())?;
        write_kv_opt_str(out, 10, "uti", attachment.uti.as_deref())?;
        write_kv_opt_str(out, 10, "mime_type", attachment.mime_type.as_deref())?;
        write_kv_i64(out, 10, "total_bytes", attachment.total_bytes)?;
        write_kv_bool(out, 10, "is_sticker", attachment.is_sticker)?;
        write_kv_i32(out, 10, "hide_attachment", attachment.hide_attachment)?;
        write_kv_opt_str(out, 10, "emoji_description", attachment.emoji_description.as_deref())?;
        write_kv_str(
            out,
            10,
            "path",
            &self.config.message_attachment_path(attachment),
        )?;

        // Sticker metadata (cheap queries only)
        if attachment.is_sticker {
            out.push_str("          sticker:\n");
            if let Some(source) = attachment.get_sticker_source(self.config.data_source.db()) {
                write_kv_str(out, 12, "source_debug", &format!("{source:?}"))?;
            } else {
                out.push_str("            source_debug: null\n");
            }
            write_kv_opt_str(
                out,
                12,
                "application_name",
                attachment
                    .get_sticker_source_application_name(self.config.data_source.db())
                    .as_deref(),
            )?;
        } else {
            out.push_str("          sticker: null\n");
        }

        out.push_str("        meta:\n");
        write_kv_opt_str(out, 10, "guid", meta.guid.as_deref())?;
        write_kv_literal_opt_str(out, 10, "transcription", meta.transcription.as_deref())?;
        write_kv_opt_f64(out, 10, "height", meta.height)?;
        write_kv_opt_f64(out, 10, "width", meta.width)?;
        write_kv_opt_str(out, 10, "name", meta.name.as_deref())?;

        Ok(())
    }

    fn write_tapbacks(&self, out: &mut String, message: &Message) -> Result<(), TableError> {
        out.push_str("    tapbacks:\n");

        let Some(tapbacks_by_part) = self.config.tapbacks.get(&message.guid) else {
            out.push_str("      []\n");
            return Ok(());
        };

        let none_destination: Option<String> = None;

        let mut wrote_any = false;
        for (part_idx, tapbacks) in tapbacks_by_part {
            for tapback in tapbacks {
                wrote_any = true;
                out.push_str("      - part_index: ");
                let _ = write!(out, "{part_idx}");
                out.push('\n');
                out.push_str("        guid: ");
                out.push_str(&yaml_quote(&tapback.guid));
                out.push('\n');
                write_kv_i32(out, 8, "rowid", tapback.rowid)?;
                write_kv_bool(out, 8, "is_from_me", tapback.is_from_me())?;
                let who = self.config.who(tapback.handle_id, tapback.is_from_me(), &none_destination);
                write_kv_str(out, 8, "sender", who)?;

                out.push_str("        timestamps:\n");
                let tapback_sent = tapback.date(&self.config.offset);
                let tapback_sent_local = tapback_sent.as_ref().ok().map(|dt| dt.to_rfc3339());
                let tapback_sent_display = if tapback.date == 0 {
                    None
                } else {
                    Some(format(&tapback_sent))
                };
                write_timestamp_parts(
                    out,
                    10,
                    "sent",
                    tapback.date,
                    tapback_sent_local.as_deref(),
                    tapback_sent_display.as_deref(),
                )?;

                // Tapback payload (associated fields)
                write_kv_opt_str(out, 8, "associated_message_guid", tapback.associated_message_guid.as_deref())?;
                write_kv_opt_i32(out, 8, "associated_message_type", tapback.associated_message_type)?;
                write_kv_opt_str(out, 8, "associated_message_emoji", tapback.associated_message_emoji.as_deref())?;

                // Variant interpretation, if possible
                out.push_str("        interpreted:\n");
                match tapback.variant() {
                    Variant::Tapback(_, action, kind) => {
                        write_kv_str(
                            out,
                            10,
                            "action",
                            match action {
                                TapbackAction::Added => "added",
                                TapbackAction::Removed => "removed",
                            },
                        )?;
                        match kind {
                            Tapback::Loved => write_kv_str(out, 10, "type", "loved")?,
                            Tapback::Liked => write_kv_str(out, 10, "type", "liked")?,
                            Tapback::Disliked => write_kv_str(out, 10, "type", "disliked")?,
                            Tapback::Laughed => write_kv_str(out, 10, "type", "laughed")?,
                            Tapback::Emphasized => write_kv_str(out, 10, "type", "emphasized")?,
                            Tapback::Questioned => write_kv_str(out, 10, "type", "questioned")?,
                            Tapback::Sticker => write_kv_str(out, 10, "type", "sticker")?,
                            Tapback::Emoji(emoji) => {
                                write_kv_str(out, 10, "type", "emoji")?;
                                write_kv_opt_str(out, 10, "emoji", emoji)?;
                            }
                        }
                    }
                    _ => {
                        write_kv_str(out, 10, "action", "unknown")?;
                        write_kv_str(out, 10, "type", "unknown")?;
                    }
                }
            }
        }

        if !wrote_any {
            out.push_str("      []\n");
        }

        Ok(())
    }

    fn write_replies(&self, out: &mut String, message: &Message) -> Result<(), TableError> {
        out.push_str("    replies:\n");

        let replies = message.get_replies(self.config.data_source.db())?;
        if replies.is_empty() {
            out.push_str("      []\n");
            return Ok(());
        }

        for (part_idx, mut reply_messages) in replies {
            for reply in &mut reply_messages {
                let _ = reply.generate_text(self.config.data_source.db());
                if reply.is_tapback() {
                    continue;
                }

                out.push_str("      - part_index: ");
                let _ = write!(out, "{part_idx}");
                out.push('\n');
                out.push_str("        guid: ");
                out.push_str(&yaml_quote(&reply.guid));
                out.push('\n');
                write_kv_i32(out, 8, "rowid", reply.rowid)?;
                write_kv_bool(out, 8, "is_from_me", reply.is_from_me())?;

                let who =
                    self.config
                        .who(reply.handle_id, reply.is_from_me(), &reply.destination_caller_id);
                write_kv_str(out, 8, "sender", who)?;

                out.push_str("        timestamps:\n");
                let reply_sent = reply.date(&self.config.offset);
                let reply_sent_local = reply_sent.as_ref().ok().map(|dt| dt.to_rfc3339());
                let reply_sent_display = if reply.date == 0 {
                    None
                } else {
                    Some(format(&reply_sent))
                };
                write_timestamp_parts(
                    out,
                    10,
                    "sent",
                    reply.date,
                    reply_sent_local.as_deref(),
                    reply_sent_display.as_deref(),
                )?;

                write_kv_literal_opt_str(out, 8, "text", reply.text.as_deref())?;

                // Cheap debug/context hooks
                write_kv_opt_str(out, 8, "thread_originator_guid", reply.thread_originator_guid.as_deref())?;
                write_kv_opt_str(out, 8, "thread_originator_part", reply.thread_originator_part.as_deref())?;

                // Attachments (best-effort; uses the same copy/conversion pipeline as other exports)
                let mut reply_attachments =
                    Attachment::from_message(self.config.data_source.db(), reply)?;
                if reply_attachments.is_empty() {
                    out.push_str("        attachments: []\n");
                } else {
                    out.push_str("        attachments:\n");
                    for attachment in &mut reply_attachments {
                        let _ = self
                            .config
                            .options
                            .attachment_manager
                            .handle_attachment(reply, attachment, self.config);
                        out.push_str("          - rowid: ");
                        let _ = write!(out, "{}", attachment.rowid);
                        out.push('\n');
                        write_kv_str(
                            out,
                            12,
                            "path",
                            &self.config.message_attachment_path(attachment),
                        )?;
                    }
                }
            }
        }

        Ok(())
    }
}

fn write_yaml_header(
    writer: &mut BufWriter<File>,
    config: &Config,
    chatroom: Option<&Chat>,
) -> Result<(), RuntimeError> {
    writeln!(writer, "---")?;
    writeln!(writer, "export:")?;
    writeln!(writer, "  generator: {}", yaml_quote("imessage-exporter"))?;
    writeln!(writer, "  version: {}", yaml_quote(env!("CARGO_PKG_VERSION")))?;
    writeln!(writer, "  platform: {}", yaml_quote(&config.options.platform.to_string()))?;
    writeln!(
        writer,
        "  db_path: {}",
        yaml_quote(&config.options.db_path.display().to_string())
    )?;
    writeln!(
        writer,
        "  export_path: {}",
        yaml_quote(&config.options.export_path.display().to_string())
    )?;
    writeln!(
        writer,
        "  copy_method: {}",
        yaml_quote(&config.options.attachment_manager.mode.to_string())
    )?;
    writeln!(writer, "conversation:")?;

    match chatroom {
        Some(chatroom) => {
            writeln!(writer, "  chat_rowid: {}", chatroom.rowid)?;
            writeln!(
                writer,
                "  chat_identifier: {}",
                yaml_quote(&chatroom.chat_identifier)
            )?;
            if let Some(service) = &chatroom.service_name {
                writeln!(writer, "  service_name: {}", yaml_quote(service))?;
            } else {
                writeln!(writer, "  service_name: null")?;
            }
            if let Some(display_name) = &chatroom.display_name {
                writeln!(writer, "  display_name: {}", yaml_quote(display_name))?;
            } else {
                writeln!(writer, "  display_name: null")?;
            }
            writeln!(writer, "  name: {}", yaml_quote(chatroom.name()))?;

            // Participants are best-effort; they depend on contacts resolution.
            writeln!(writer, "  participants:")?;
            if let Some(handle_ids) = config.chatroom_participants.get(&chatroom.rowid) {
                let none_destination: Option<String> = None;
                for handle_id in handle_ids {
                    let who = config.who(Some(*handle_id), false, &none_destination);
                    writeln!(writer, "    - {}", yaml_quote(who))?;
                }
            } else {
                writeln!(writer, "    - {}", yaml_quote("Unknown"))?;
            }
        }
        None => {
            writeln!(writer, "  orphaned: true")?;
            writeln!(writer, "  name: {}", yaml_quote("orphaned"))?;
        }
    }

    writeln!(writer, "messages:")?;
    Ok(())
}

fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(&mut out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn indent(out: &mut String, spaces: usize) {
    out.extend(std::iter::repeat_n(' ', spaces));
}

fn write_kv_str(out: &mut String, spaces: usize, key: &str, value: &str) -> Result<(), TableError> {
    indent(out, spaces);
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&yaml_quote(value));
    out.push('\n');
    Ok(())
}

fn write_kv_opt_str(
    out: &mut String,
    spaces: usize,
    key: &str,
    value: Option<&str>,
) -> Result<(), TableError> {
    match value {
        Some(v) => write_kv_str(out, spaces, key, v),
        None => {
            indent(out, spaces);
            out.push_str(key);
            out.push_str(": null\n");
            Ok(())
        }
    }
}

fn write_kv_literal_str(
    out: &mut String,
    spaces: usize,
    key: &str,
    value: &str,
) -> Result<(), TableError> {
    indent(out, spaces);
    out.push_str(key);
    out.push_str(": |\n");
    for line in value.split('\n') {
        indent(out, spaces + 2);
        out.push_str(line);
        out.push('\n');
    }
    Ok(())
}

fn write_kv_literal_opt_str(
    out: &mut String,
    spaces: usize,
    key: &str,
    value: Option<&str>,
) -> Result<(), TableError> {
    match value {
        Some(v) => write_kv_literal_str(out, spaces, key, v),
        None => {
            indent(out, spaces);
            out.push_str(key);
            out.push_str(": null\n");
            Ok(())
        }
    }
}

fn write_kv_i32(out: &mut String, spaces: usize, key: &str, value: i32) -> Result<(), TableError> {
    indent(out, spaces);
    let _ = write!(out, "{key}: {value}\n");
    Ok(())
}

fn write_kv_opt_i32(
    out: &mut String,
    spaces: usize,
    key: &str,
    value: Option<i32>,
) -> Result<(), TableError> {
    match value {
        Some(v) => write_kv_i32(out, spaces, key, v),
        None => {
            indent(out, spaces);
            out.push_str(key);
            out.push_str(": null\n");
            Ok(())
        }
    }
}

fn write_kv_i64(out: &mut String, spaces: usize, key: &str, value: i64) -> Result<(), TableError> {
    indent(out, spaces);
    let _ = write!(out, "{key}: {value}\n");
    Ok(())
}

fn write_kv_opt_f64(
    out: &mut String,
    spaces: usize,
    key: &str,
    value: Option<f64>,
) -> Result<(), TableError> {
    match value {
        Some(v) => {
            indent(out, spaces);
            let _ = write!(out, "{key}: {v}\n");
            Ok(())
        }
        None => {
            indent(out, spaces);
            out.push_str(key);
            out.push_str(": null\n");
            Ok(())
        }
    }
}

fn write_kv_bool(out: &mut String, spaces: usize, key: &str, value: bool) -> Result<(), TableError> {
    indent(out, spaces);
    let _ = write!(out, "{key}: {}\n", if value { "true" } else { "false" });
    Ok(())
}

fn write_timestamp_parts(
    out: &mut String,
    spaces: usize,
    key: &str,
    raw: i64,
    local_rfc3339: Option<&str>,
    display: Option<&str>,
) -> Result<(), TableError> {
    indent(out, spaces);
    out.push_str(key);
    out.push_str(":\n");
    write_kv_i64(out, spaces + 2, "raw", raw)?;

    if raw == 0 {
        indent(out, spaces + 2);
        out.push_str("local: null\n");
        indent(out, spaces + 2);
        out.push_str("display: null\n");
        return Ok(());
    }

    match local_rfc3339 {
        Some(local_rfc3339) => write_kv_str(out, spaces + 2, "local", local_rfc3339)?,
        None => write_kv_opt_str(out, spaces + 2, "local", None)?,
    };
    match display {
        Some(display) => write_kv_str(out, spaces + 2, "display", display)?,
        None => write_kv_opt_str(out, spaces + 2, "display", None)?,
    };
    Ok(())
}

fn extract_text_segment(text: &str, attributes: &[TextAttributes]) -> String {
    if text.is_empty() || attributes.is_empty() {
        return String::new();
    }

    let mut rendered = String::with_capacity(text.len());
    let mut prev_start = 0;
    let mut prev_end = 0;

    for attr in attributes {
        if prev_start == attr.start && prev_end == attr.end {
            continue;
        }
        prev_start = attr.start;
        prev_end = attr.end;

        if let Some(slice) = text.get(attr.start..attr.end) {
            rendered.push_str(slice);
        }
    }

    rendered
}
