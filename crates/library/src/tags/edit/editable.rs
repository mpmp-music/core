use super::*;

impl Editable {
    /// What the library shows of the file's tags (`forced`: its root's encoding for Latin-1
    /// ID3 text): every tag, values cut as the library cuts them, those it does not keep
    /// left out.
    pub fn shown(&self, forced: Option<&'static Encoding>) -> TagValues {
        let tags = self.tags();
        let pairs = read::text_tags_in(
            self.primary.tag_type(),
            &tags,
            &self.primary.raw(),
            self.decoding_of(&tags, forced).0,
            read::MAX_TAG_VALUE,
        );
        grouped(
            pairs
                .into_iter()
                .filter(|(_, v)| !normalize(v).is_empty())
                .collect(),
        )
    }

    /// The primary tag, then the others: all the library reads of the file.
    fn tags(&self) -> Vec<Tag> {
        let mut tags = vec![self.primary.generic()];
        tags.extend(self.others.iter().cloned());
        tags
    }

    fn decoding_of(&self, tags: &[Tag], forced: Option<&'static Encoding>) -> Decoding {
        Decoding(
            read::latin1_encoding(self.primary.tag_type(), tags, &self.primary.raw(), forced),
            forced,
        )
    }

    /// How the file's Latin-1 ID3 text reads as it is now (`forced`: its root's encoding),
    /// as [`Editable::shown`] reads it. Every part of the file is read in it (an item on
    /// its own too), so that what an edit keeps, carries over and writes back is what the
    /// library shows.
    pub fn decoding(&self, forced: Option<&'static Encoding>) -> Decoding {
        self.decoding_of(&self.tags(), forced)
    }

    /// Every value the primary tag holds, by key, whole (not cut).
    fn values(&self, decoding: Decoding) -> TagValues {
        grouped(read::text_tags_in(
            self.primary.tag_type(),
            &[self.primary.generic()],
            &self.primary.raw(),
            decoding.0,
            usize::MAX,
        ))
    }

    /// The values the primary tag holds for `keys`, whole (not cut): what undoing a change
    /// of them writes back (no values: the key was not there).
    pub fn primary_values(&self, keys: &[&str], decoding: Decoding) -> Vec<(String, Vec<String>)> {
        let all = self.values(decoding);
        keys.iter()
            .map(|k| ((*k).to_owned(), all.get(*k).cloned().unwrap_or_default()))
            .collect()
    }

    /// What item `i` of the primary tag shows on its own, read as `decoding`.
    fn item_values(&self, i: usize, decoding: Decoding) -> TagValues {
        let single = self.primary.single(i);
        grouped(read::text_tags_in(
            single.tag_type(),
            &[single.generic()],
            &single.raw(),
            decoding.0,
            usize::MAX,
        ))
    }

    /// Applies checked changes ([`check_changes`]) to the primary tag. `true` if it
    /// changed.
    fn apply(&mut self, changes: &[TagChange], decoding: Decoding) -> Result<bool, String> {
        // (Not by comparing the tags: ID3v2 frames compare by id only.)
        let mut changed = false;
        for change in changes {
            changed |= self.apply_one(change.key(), change.values(), decoding)?;
        }
        Ok(changed)
    }

    fn apply_one(
        &mut self,
        key: &str,
        values: &[String],
        decoding: Decoding,
    ) -> Result<bool, String> {
        let n = self.primary.len();
        let mut keep = vec![true; n];
        let mut carried: Vec<(String, Vec<String>)> = Vec::new();
        let mut native = Vec::new();
        let mut name = None;
        let mut current: Vec<String> = Vec::new();
        let mut values = values.to_vec();
        let home = (!values.is_empty())
            .then(|| self.primary.home(key))
            .flatten();
        for (i, kept) in keep.iter_mut().enumerate() {
            // Read as the whole file reads it: what is kept, carried over or compared is
            // what the library shows.
            let shows = self.item_values(i, decoding);
            let none = Vec::new();
            let mine = match shows.get(key) {
                Some(mine) => mine,
                // The item the key goes into, holding other keys only (an ID3v2 `TRCK` with
                // the track number, for the total): written again with them, as the new
                // item would replace it.
                None if home.is_some() && self.primary.ident(i) == home => &none,
                None => continue,
            };
            let shared = self.primary.shared_rest(i);
            if self.primary.kept(i) && shared.is_none() {
                if home.is_some() && self.primary.ident(i) == home && mine != &values {
                    return Err(format!(
                        "rebuilding {key} would replace protected native data"
                    ));
                }
                // Displayed values include these protected items. Do not copy them into
                // ordinary comments/lyrics when saving displayed values or undoing.
                for value in mine {
                    if let Some(at) = values.iter().position(|v| v.trim() == value.trim()) {
                        values.remove(at);
                    }
                }
                continue;
            }
            if let Some(rest) = shared {
                native.push(rest);
            }
            *kept = false;
            current.extend(mine.iter().cloned());
            if name.is_none() {
                name = self.primary.name(i, key).map(|n| (key.to_owned(), n));
            }
            for (other, more) in shows {
                if other == key {
                    continue;
                }
                match carried.iter_mut().find(|(k, _)| *k == other) {
                    Some((_, vs)) => vs.extend(more),
                    None => carried.push((other, more)),
                }
            }
        }
        // The same values already: nothing to do (an ID3v2 frame would otherwise lose its
        // text encoding, say; one holding other keys too would be written again for
        // nothing).
        if current == values {
            return Ok(false);
        }
        if decoding.0.is_none()
            && native.iter().any(|rest| {
                rest.into_iter().any(|frame| {
                    matches!(frame, Frame::KeyValue(t) if t.encoding == TextEncoding::Latin1
                && t.key_value_pairs.iter().any(|(k, v)| !k.is_ascii() || !v.is_ascii()))
                })
            })
        {
            return Err(
                "the encoding of native people roles is unknown; set the root encoding first"
                    .into(),
            );
        }
        self.primary.retain(&keep);
        let mut entries = carried;
        if !values.is_empty() {
            entries.push((key.to_owned(), values.to_vec()));
        }
        self.primary.add(&entries, name.as_ref());
        for rest in native {
            self.primary.restore_shared(rest);
        }
        Ok(true)
    }

    /// Applies changes, checks conversion into native items, and reads serialized ID3
    /// bytes back before reporting a preview or writing any file. Changes use the old
    /// `decoding`; verification detects again after the edit. Requested values, all other
    /// displayed values, and native people roles must survive.
    pub fn apply_verified(
        &mut self,
        changes: &[TagChange],
        decoding: Decoding,
    ) -> Result<bool, String> {
        let untouched = |values: TagValues| -> TagValues {
            values
                .into_iter()
                .filter(|(key, _)| !changes.iter().any(|c| c.key() == key))
                .map(|(key, mut values)| {
                    values.sort();
                    (key, values)
                })
                .collect()
        };
        let original = self.primary.clone();
        let others = untouched(self.values(decoding));
        let shown_others = untouched(self.shown(decoding.1));
        let mut expected = self.primary_values(
            &changes.iter().map(TagChange::key).collect::<Vec<_>>(),
            decoding,
        );
        for ((key, values), change) in expected.iter_mut().zip(changes) {
            let mut kept = Vec::new();
            for i in 0..self.primary.len() {
                if self.primary.kept(i) && self.primary.shared_rest(i).is_none() {
                    let shown = self.item_values(i, decoding);
                    kept.extend(shown.get(key).into_iter().flatten().cloned());
                }
            }
            *values = change.values().to_vec();
            // Retained items always remain, even when absent from a Set or Remove.
            let mut requested = values.clone();
            for value in kept {
                if let Some(at) = requested.iter().position(|v| v.trim() == value.trim()) {
                    requested.remove(at);
                } else {
                    values.push(value);
                }
            }
        }
        let changed = self.apply(changes, decoding)?;
        if !changed {
            return Ok(false);
        }
        self.primary = original;
        let converts = self.converts_encoding(decoding);
        if converts {
            self.rewrite_latin1(decoding)?;
        }
        let native_roles = self.primary.native_roles();
        self.apply(changes, self.decoding(decoding.1))?;
        // NUL-separated multi-values belong to ID3v2.4, not 2.3.
        if changes.iter().any(|c| c.values().len() > 1) {
            self.id3v23 = false;
        }
        if let Native::Id3v2(tag) = &self.primary {
            let mut bytes = Vec::new();
            tag.dump_to(&mut bytes, WriteOptions::new().use_id3v23(self.id3v23))
                .map_err(|e| e.to_string())?;
            if bytes.is_empty() {
                self.primary = Native::Id3v2(Id3v2Tag::new());
            } else {
                // MPEG's tag-only reader needs a tail for its end-of-file tag probes.
                bytes.extend([0; 128]);
                let parsed = MpegFile::read_from(
                    &mut Cursor::new(bytes),
                    ParseOptions::new().read_properties(false),
                )
                .map_err(|e| e.to_string())?;
                self.primary = Native::Id3v2(parsed.id3v2().cloned().unwrap_or_default());
            }
        }
        if self.primary.native_roles() != native_roles {
            return Err("the change would alter native people roles".into());
        }
        let keys: Vec<&str> = changes.iter().map(TagChange::key).collect();
        let after = self.decoding(decoding.1);
        let actual = self.primary_values(&keys, after);
        for ((key, mut expected), (_, mut actual)) in expected.into_iter().zip(actual) {
            expected.sort();
            actual.sort();
            if actual != expected {
                return Err(format!(
                    "the file's tag format cannot store {key} with these values"
                ));
            }
        }
        let now = untouched(self.values(after));
        if untouched(self.shown(decoding.1)) != shown_others {
            return Err("the change would alter other displayed values".into());
        }
        if let Some(key) = others
            .keys()
            .chain(now.keys())
            .find(|key| others.get(*key) != now.get(*key))
        {
            return Err(format!(
                "the change would also change {key}, which it does not name"
            ));
        }
        Ok(true)
    }

    /// Convert every Latin-1 text component before shared frames are rebuilt. A single
    /// undecodable component refuses the file, including native role names and values.
    fn rewrite_latin1(&mut self, decoding: Decoding) -> Result<(), String> {
        let Some(encoding) = decoding.0 else {
            return Ok(());
        };
        let Native::Id3v2(tag) = &self.primary else {
            return Ok(());
        };
        let mut frames: Vec<_> = tag.into_iter().cloned().collect();
        let repair = |value: &mut std::borrow::Cow<'static, str>| -> Result<(), String> {
            let bytes = value
                .chars()
                .map(|c| u8::try_from(u32::from(c)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "invalid Latin-1 text")?;
            let text = encoding
                .decode_without_bom_handling_and_without_replacement(&bytes)
                .ok_or("legacy text cannot be decoded without replacement")?;
            *value = text.into_owned().into();
            Ok(())
        };
        for frame in &mut frames {
            match frame {
                Frame::Text(t) if t.encoding == TextEncoding::Latin1 => {
                    repair(&mut t.value)?;
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::UserText(t) if t.encoding == TextEncoding::Latin1 => {
                    repair(&mut t.description)?;
                    repair(&mut t.content)?;
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::KeyValue(t) if t.encoding == TextEncoding::Latin1 => {
                    for (role, value) in t.key_value_pairs.to_mut() {
                        repair(role)?;
                        repair(value)?;
                    }
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::Comment(t) if t.encoding == TextEncoding::Latin1 => {
                    repair(&mut t.description)?;
                    repair(&mut t.content)?;
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::UnsynchronizedText(t) if t.encoding == TextEncoding::Latin1 => {
                    repair(&mut t.description)?;
                    repair(&mut t.content)?;
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::UserUrl(t) if t.encoding == TextEncoding::Latin1 => {
                    repair(&mut t.description)?;
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::Ownership(t) if t.encoding == TextEncoding::Latin1 => {
                    repair(&mut t.seller)?;
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::Timestamp(t) if t.encoding == TextEncoding::Latin1 => {
                    t.encoding = TextEncoding::UTF8;
                }
                Frame::Picture(t) if t.encoding == TextEncoding::Latin1 => {
                    if let Some(description) = t.picture.description() {
                        let mut description = std::borrow::Cow::Owned(description.to_owned());
                        repair(&mut description)?;
                        t.picture
                            .to_mut()
                            .set_description(Some(description.into_owned()));
                    }
                    t.encoding = TextEncoding::UTF8;
                }
                // Opaque and nested text cannot be validated by this editor.
                Frame::Binary(t)
                    if matches!(t.id().as_str(), "SYLT" | "GEOB" | "ATXT")
                        || t.id().as_str().starts_with('T') =>
                {
                    return Err("opaque legacy text cannot be safely converted".into());
                }
                Frame::Chapter(_) | Frame::TableOfContents(_) => {
                    return Err("nested legacy text cannot be safely converted".into());
                }
                _ => {}
            }
        }
        let Native::Id3v2(tag) = &mut self.primary else {
            unreachable!()
        };
        tag.retain(|_| false);
        for frame in frames {
            tag.insert(frame);
        }
        Ok(())
    }

    pub fn serialized_id3(&self) -> Result<Vec<u8>, String> {
        let Native::Id3v2(tag) = &self.primary else {
            return Err("not ID3".into());
        };
        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::new().use_id3v23(self.id3v23))
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    pub fn converts_encoding(&self, decoding: Decoding) -> bool {
        if decoding.0.is_none() {
            return false;
        }
        let Native::Id3v2(tag) = &self.primary else {
            return false;
        };
        fn legacy(frame: &Frame<'_>) -> bool {
            match frame {
                Frame::Text(t) => t.encoding == TextEncoding::Latin1 && !t.value.is_ascii(),
                Frame::UserText(t) => {
                    t.encoding == TextEncoding::Latin1
                        && (!t.description.is_ascii() || !t.content.is_ascii())
                }
                Frame::KeyValue(t) => {
                    t.encoding == TextEncoding::Latin1
                        && t.key_value_pairs
                            .iter()
                            .any(|(k, v)| !k.is_ascii() || !v.is_ascii())
                }
                Frame::Comment(t) => {
                    t.encoding == TextEncoding::Latin1
                        && (!t.description.is_ascii() || !t.content.is_ascii())
                }
                Frame::UnsynchronizedText(t) => {
                    t.encoding == TextEncoding::Latin1
                        && (!t.description.is_ascii() || !t.content.is_ascii())
                }
                Frame::UserUrl(t) => {
                    t.encoding == TextEncoding::Latin1 && !t.description.is_ascii()
                }
                Frame::Ownership(t) => t.encoding == TextEncoding::Latin1 && !t.seller.is_ascii(),
                Frame::Binary(t) => {
                    t.data.first() == Some(&0)
                        && (matches!(t.id().as_str(), "SYLT" | "GEOB" | "ATXT")
                            || t.id().as_str().starts_with('T'))
                }
                Frame::Picture(t) => {
                    t.encoding == TextEncoding::Latin1
                        && t.picture.description().is_some_and(|d| !d.is_ascii())
                }
                Frame::Chapter(t) => t.children.iter().any(legacy),
                Frame::TableOfContents(t) => t.children.iter().any(legacy),
                _ => false,
            }
        }
        tag.into_iter().any(legacy)
    }

    /// Checks that what was applied can be undone: that the primary tag, as an undo will
    /// find it in the file (and read it, `forced`: the root's encoding), can be given
    /// `before` again (what [`Editable::primary_values`] read before the change).
    #[cfg(all(test, feature = "desktop-fs"))]
    pub fn undo_check(
        &self,
        before: &[(String, Vec<String>)],
        forced: Option<&'static Encoding>,
    ) -> Result<(), String> {
        let mut back = Editable {
            format: self.format,
            file_type: self.file_type,
            primary: self.primary.clone(),
            others: self.others.clone(),
            id3v23: self.id3v23,
        };
        let decoding = back.decoding(forced);
        back.apply_verified(&reverse_changes(before), decoding)
            .map(drop)
    }

    /// Checks that writing and reading a copy preserved the primary tag's values (every
    /// key's, read as `decoding`).
    pub fn same_values(&self, other: &Self, decoding: Decoding) -> bool {
        self.values(self.decoding(decoding.1)) == other.values(other.decoding(decoding.1))
            && self.shown(decoding.1) == other.shown(decoding.1)
            && self.primary.native_roles() == other.primary.native_roles()
    }

    /// Test helper that materializes the production metadata plan.
    #[cfg(all(test, feature = "desktop-fs"))]
    pub fn save(&self, file: &mut File) -> Result<(), String> {
        let staged = (|| {
            let mut temp = tempfile::tempfile()?;
            self.plan(file)?.render(file, &mut temp)?;
            file.set_len(0)?;
            file.rewind()?;
            temp.rewind()?;
            std::io::copy(&mut temp, file)?;
            Ok::<_, std::io::Error>(())
        })();
        staged.map_err(|error| error.to_string())
    }
}
