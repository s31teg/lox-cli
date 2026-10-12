use super::{ConfigEditor, UTF8_BOM};
use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, TimeZone};

/// Whole seconds since the Loxone epoch (2009-01-01 00:00:00 UTC) — the `DateS` format.
pub fn loxone_seconds<Tz: TimeZone>(t: &DateTime<Tz>) -> i64 {
    t.timestamp() - crate::client::LOXONE_EPOCH_SECS as i64
}

impl ConfigEditor {
    /// Bump the Document save stamp (`Date` / `DateS` on `<C Type="Document">`) to `now`.
    ///
    /// Loxone Config decides whether an opened file is "the same program" as the one on
    /// the Miniserver by this stamp, not by content: an edited file that keeps the old
    /// stamp is treated as identical and Config loads the Miniserver's program instead,
    /// silently discarding the edits. `Date` is the wall-clock time of `now`
    /// (`%Y-%m-%d %H:%M:%S`); `DateS` is whole seconds since the Loxone epoch, computed
    /// from the same instant in UTC. No other attribute is touched and attribute order
    /// is preserved. Returns `false` if the file has no Document element.
    ///
    /// Deliberately separate from [`ConfigEditor::to_bytes`], which stays a pure
    /// serialisation so unedited round-trips remain byte-identical.
    pub fn stamp_document(&mut self, now: &DateTime<FixedOffset>) -> bool {
        let Some(doc) = self
            .root
            .children
            .iter_mut()
            .filter_map(|node| node.as_mut_element())
            .find(|e| {
                e.name == "C" && e.attributes.get("Type").map(|s| s.as_str()) == Some("Document")
            })
        else {
            return false;
        };
        doc.attributes.insert(
            "Date".to_string(),
            now.format("%Y-%m-%d %H:%M:%S").to_string(),
        );
        doc.attributes
            .insert("DateS".to_string(), loxone_seconds(now).to_string());
        true
    }

    /// Write the edited XML back to bytes, preserving BOM and line endings.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Write XML declaration
        buf.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");

        // Write DOM tree
        let config = xmltree::EmitterConfig::new()
            .perform_indent(true)
            .indent_string("\t")
            .write_document_declaration(false)
            // Loxone writes self-closing tags without a leading space (`<C/>`, not `<C />`).
            // xml-rs pads by default, which makes every self-closing element differ on
            // round-trip and drowns real changes in formatting noise.
            .pad_self_closing(false);
        self.root
            .write_with_config(&mut buf, config)
            .context("Failed to write XML")?;

        // Post-process: restore digit-prefixed attribute names
        if !self.digit_attr_renames.is_empty() {
            let mut s = String::from_utf8(buf).context("XML is not valid UTF-8")?;
            for (sanitized, original) in &self.digit_attr_renames {
                let from = format!(" {sanitized}=");
                let to = format!(" {original}=");
                s = s.replace(&from, &to);
            }
            buf = s.into_bytes();
        }

        // Post-process XML syntax emitted by xml-rs to match Loxone's formatting.
        {
            let s = String::from_utf8(buf).context("XML is not valid UTF-8")?;
            let mut s = Self::normalize_loxone_xml(&s);
            // Loxone terminates the file with a trailing newline; xml-rs does not.
            if !s.ends_with('\n') {
                s.push('\n');
            }
            buf = s.into_bytes();
        }

        // Post-process: restore BOM
        if self.had_bom {
            let mut result = Vec::with_capacity(3 + buf.len());
            result.extend_from_slice(UTF8_BOM);
            result.extend_from_slice(&buf);
            buf = result;
        }

        // Post-process: restore CRLF line endings
        if self.had_crlf {
            let s = String::from_utf8(buf).context("XML is not valid UTF-8")?;
            buf = s.replace('\n', "\r\n").into_bytes();
        }

        Ok(buf)
    }

    /// Match Loxone's empty-element and attribute-newline formatting.
    ///
    /// Only start tags are rewritten so XML-like text in comments, CDATA, and processing
    /// instructions remains unchanged.
    fn normalize_loxone_xml(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(bytes.len());
        let mut last = 0;
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] != b'<' {
                i += 1;
                continue;
            }

            let tail = &s[i..];
            let skipped_end = if tail.starts_with("<!--") {
                tail.find("-->").map(|end| i + end + 3)
            } else if tail.starts_with("<![CDATA[") {
                tail.find("]]>").map(|end| i + end + 3)
            } else if tail.starts_with("<?") {
                tail.find("?>").map(|end| i + end + 2)
            } else if tail.starts_with("</") || tail.starts_with("<!") {
                Self::xml_tag_end(bytes, i)
            } else {
                None
            };
            if let Some(end) = skipped_end {
                i = end;
                continue;
            }

            let Some(end) = Self::xml_tag_end(bytes, i) else {
                break;
            };
            out.push_str(&s[last..i]);
            out.push_str(&Self::normalize_start_tag(&s[i..end]));
            i = end;
            last = i;
        }

        out.push_str(&s[last..]);
        out
    }

    fn xml_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
        let mut quote = None;
        for (i, &byte) in bytes.iter().enumerate().skip(start + 1) {
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    quote = None;
                }
            } else if matches!(byte, b'"' | b'\'') {
                quote = Some(byte);
            } else if byte == b'>' {
                return Some(i + 1);
            }
        }
        None
    }

    fn normalize_start_tag(tag: &str) -> String {
        let bytes = tag.as_bytes();
        let mut name_end = 1;
        while name_end < bytes.len()
            && (bytes[name_end].is_ascii_alphanumeric()
                || matches!(bytes[name_end], b'_' | b':' | b'-' | b'.'))
        {
            name_end += 1;
        }

        // Loxone expands attribute-less empty elements but keeps attributed empties closed.
        if name_end + 2 == bytes.len() && bytes[name_end] == b'/' && bytes[name_end + 1] == b'>' {
            let name = &tag[1..name_end];
            return format!("<{name}></{name}>");
        }

        // xml-rs escapes newlines in attributes; Loxone writes them literally.
        let mut out = String::with_capacity(tag.len());
        let mut quote = None;
        let mut last = 0;
        let mut i = name_end;
        while i < bytes.len() {
            if let Some(delimiter) = quote {
                if bytes[i..].starts_with(b"&#xA;") {
                    out.push_str(&tag[last..i]);
                    out.push('\n');
                    i += 5;
                    last = i;
                    continue;
                }
                if bytes[i] == delimiter {
                    quote = None;
                }
            } else if matches!(bytes[i], b'"' | b'\'') {
                quote = Some(bytes[i]);
            }
            i += 1;
        }
        out.push_str(&tag[last..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigEditor, loxone_seconds};
    use chrono::{FixedOffset, TimeZone, Utc};

    const STAMP_XML: &str = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
<ControlList Version=\"254\" LxAV=\"76\">\r\n\
\t<C Type=\"Document\" V=\"16000603\" U=\"doc-1\" Title=\"Home\" Date=\"2025-06-10 09:02:40\" CDate=\"\" BDate=\"\" DateS=\"518770960\" Ctry=\"DE\" 12hTF=\"true\" NumO=\"424\">\r\n\
\t\t<C Type=\"Place\" U=\"room-1\" Title=\"Kitchen\"/>\r\n\
\t</C>\r\n\
</ControlList>\r\n";

    #[test]
    fn test_loxone_seconds_matches_real_files() {
        // Europe/London BST (UTC+1), taken from a real Loxone Config save.
        let bst = FixedOffset::east_opt(3600).unwrap();
        let t = bst.with_ymd_and_hms(2026, 10, 12, 7, 55, 33).unwrap();
        assert_eq!(loxone_seconds(&t), 561_020_133);
        // CEST (UTC+2), from a sample config's Document element.
        let cest = FixedOffset::east_opt(7200).unwrap();
        let t = cest.with_ymd_and_hms(2025, 6, 10, 9, 2, 40).unwrap();
        assert_eq!(loxone_seconds(&t), 518_770_960);
        // The epoch itself.
        let epoch = Utc.with_ymd_and_hms(2009, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(loxone_seconds(&epoch), 0);
    }

    #[test]
    fn test_stamp_document_changes_only_date_and_dates() {
        let mut editor = ConfigEditor::load(STAMP_XML.as_bytes()).unwrap();
        let before = String::from_utf8(editor.to_bytes().unwrap()).unwrap();
        assert_eq!(
            before, STAMP_XML,
            "fixture must round-trip byte-identically"
        );

        let bst = FixedOffset::east_opt(3600).unwrap();
        let now = bst.with_ymd_and_hms(2026, 10, 12, 7, 55, 33).unwrap();
        assert!(editor.stamp_document(&now));
        let after = String::from_utf8(editor.to_bytes().unwrap()).unwrap();

        let expected = before
            .replace(
                "Date=\"2025-06-10 09:02:40\"",
                "Date=\"2026-10-12 07:55:33\"",
            )
            .replace("DateS=\"518770960\"", "DateS=\"561020133\"");
        assert_ne!(before, expected);
        assert_eq!(after, expected);
    }

    #[test]
    fn test_stamp_document_without_document_is_noop() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ControlList Version=\"1\">\n\t<C Type=\"Place\" U=\"r\" Title=\"R\"/>\n</ControlList>\n";
        let mut editor = ConfigEditor::load(xml.as_bytes()).unwrap();
        let now = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap();
        assert!(!editor.stamp_document(&now));
        assert_eq!(String::from_utf8(editor.to_bytes().unwrap()).unwrap(), xml);
    }

    #[test]
    fn test_write_matches_loxone_formatting() {
        // Loxone's on-disk conventions the emitter must reproduce for byte-clean round-trips:
        // no space before '/>', attribute-less empties expanded, attributed empties
        // self-closed, literal newlines in attribute values, trailing newline.
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<ControlList Version=\"1\">\n\
\t<C U=\"a\" NTXT=\"line1\nline2\">\n\
\t\t<IoData></IoData>\n\
\t\t<Co K=\"Q\" U=\"b\"></Co>\n\
\t</C>\n\
</ControlList>\n";
        let editor = ConfigEditor::load(xml.as_bytes()).unwrap();
        let out = String::from_utf8(editor.to_bytes().unwrap()).unwrap();
        assert!(!out.contains(" />"), "no padded self-close");
        assert!(
            out.contains("<IoData></IoData>"),
            "attr-less empty stays expanded"
        );
        assert!(
            out.contains(r#"<Co K="Q" U="b"/>"#),
            "attributed empty self-closes"
        );
        assert!(
            out.contains("line1\nline2"),
            "literal newline in attr value"
        );
        assert!(!out.contains("&#xA;"), "no escaped newline");
        assert!(out.ends_with('\n'), "trailing newline");
    }

    #[test]
    fn test_write_preserves_comment_and_cdata_payloads() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<ControlList Version=\"1\">\n\
\t<!-- preserve <IoData/> and &#xA; literally -->\n\
\t<![CDATA[preserve <IoData/> and &#xA; literally]]>\n\
</ControlList>\n";
        let editor = ConfigEditor::load(xml.as_bytes()).unwrap();
        let out = String::from_utf8(editor.to_bytes().unwrap()).unwrap();

        assert!(out.contains("<!-- preserve <IoData/> and &#xA; literally -->"));
        assert!(out.contains("<![CDATA[preserve <IoData/> and &#xA; literally]]>"));
    }
}
