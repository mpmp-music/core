use super::*;

impl Native {
    pub(super) fn tag_type(&self) -> TagType {
        match self {
            Native::Vorbis(_) => TagType::VorbisComments,
            Native::Id3v2(_) => TagType::Id3v2,
            Native::Ilst(_) => TagType::Mp4Ilst,
        }
    }

    pub(super) fn generic(&self) -> Tag {
        match self {
            Native::Vorbis(t) => t.clone().into(),
            Native::Id3v2(t) => t.clone().into(),
            Native::Ilst(t) => t.clone().into(),
        }
    }

    pub(super) fn raw(&self) -> RawTags {
        match self {
            Native::Vorbis(t) => raw_vorbis(t),
            Native::Id3v2(t) => raw_id3v2(Some(t)),
            Native::Ilst(t) => raw_ilst(t),
        }
    }

    /// Its items (comments, frames, atoms).
    pub(super) fn len(&self) -> usize {
        match self {
            Native::Vorbis(t) => t.items().len(),
            Native::Id3v2(t) => t.len(),
            Native::Ilst(t) => t.len(),
        }
    }

    /// A tag of the same type holding only item `i`.
    pub(super) fn single(&self, i: usize) -> Native {
        match self {
            Native::Vorbis(t) => {
                let mut one = VorbisComments::new();
                if let Some((k, v)) = t.items().nth(i) {
                    one.push(k.to_owned(), v.to_owned());
                }
                Native::Vorbis(one)
            }
            Native::Id3v2(t) => {
                let mut one = Id3v2Tag::new();
                if let Some(frame) = t.into_iter().nth(i) {
                    one.insert(frame.clone());
                }
                Native::Id3v2(one)
            }
            Native::Ilst(t) => {
                let mut one = Ilst::new();
                if let Some(atom) = t.into_iter().nth(i) {
                    one.insert(atom.clone());
                }
                Native::Ilst(one)
            }
        }
    }

    /// Whether item `i` stays whatever key it shows: an ID3v2 comment or lyrics with a
    /// description, or a frame that holds more than the library shows of it.
    pub(super) fn kept(&self, i: usize) -> bool {
        let Native::Id3v2(t) = self else {
            return false;
        };
        let Some(frame) = t.into_iter().nth(i) else {
            return true;
        };
        if let Frame::Comment(c) = frame
            && !c.description.is_empty()
        {
            return true;
        }
        if let Frame::UnsynchronizedText(u) = frame
            && !u.description.is_empty()
        {
            return true;
        }
        let mut one = Id3v2Tag::new();
        one.insert(frame.clone());
        let (rest, generic) = one.split_tag();
        // People frames can consist entirely of native roles the library does not show.
        !Id3v2Tag::from(rest).is_empty()
            && (generic.items().next().is_some() || matches!(frame, Frame::KeyValue(_)))
    }

    /// The native roles a shared people frame's generic mapping does not consume.
    pub(super) fn shared_rest(&self, i: usize) -> Option<Id3v2Tag> {
        let Native::Id3v2(t) = self else { return None };
        let frame = t.into_iter().nth(i)?;
        if !matches!(frame, Frame::KeyValue(_)) {
            return None;
        }
        let mut one = Id3v2Tag::new();
        one.insert(frame.clone());
        let (rest, _) = one.split_tag();
        Some(Id3v2Tag::from(rest))
    }

    /// Unknown people roles, checked independently of the displayed values.
    pub(super) fn native_roles(&self) -> Vec<(String, String, String)> {
        let mut roles = Vec::new();
        for i in 0..self.len() {
            if let Some(rest) = self.shared_rest(i) {
                for frame in rest {
                    if let Frame::KeyValue(list) = frame {
                        for (role, value) in list.key_value_pairs.iter() {
                            roles.push((
                                list.id().as_str().to_owned(),
                                role.to_string(),
                                value.to_string(),
                            ));
                        }
                    }
                }
            }
        }
        roles.sort();
        roles
    }

    /// Merge native roles into a rebuilt people frame, including a frame with no known roles.
    pub(super) fn restore_shared(&mut self, rest: Id3v2Tag) {
        let Native::Id3v2(tag) = self else { return };
        for frame in rest {
            let Frame::KeyValue(mut native) = frame else {
                unreachable!()
            };
            if let Some(Frame::KeyValue(rebuilt)) =
                (&*tag).into_iter().find(|f| f.id() == native.id())
            {
                native
                    .key_value_pairs
                    .to_mut()
                    .extend(rebuilt.key_value_pairs.iter().cloned());
                native.encoding = TextEncoding::UTF8;
            }
            tag.retain(|frame| frame.id() != native.id());
            tag.insert(Frame::KeyValue(native));
        }
    }

    /// The name item `i` has, to write `key` again under it: a Vorbis comment's, a `TXXX`
    /// frame's description, an MP4 atom's ident for a key lofty does not map (a mapped one
    /// is written as lofty writes it: `trkn` holds numbers, not text).
    pub(super) fn name(&self, i: usize, key: &str) -> Option<ItemName> {
        match self {
            Native::Vorbis(t) => t
                .items()
                .nth(i)
                .map(|(k, _)| ItemName::Vorbis(k.to_owned())),
            Native::Id3v2(t) => match t.into_iter().nth(i)? {
                Frame::UserText(text) => Some(ItemName::UserText(text.description.to_string())),
                _ => None,
            },
            Native::Ilst(_) if mapped_key(key).is_some() => None,
            Native::Ilst(t) => t
                .into_iter()
                .nth(i)
                .map(|atom| ItemName::Atom(atom.ident().clone().into_owned())),
        }
    }

    /// The id of item `i` when it is the only one of its id a tag holds (an ID3v2 text or
    /// involved people frame, an MP4 atom of a four-character code).
    pub(super) fn ident(&self, i: usize) -> Option<String> {
        match self {
            Native::Vorbis(_) => None,
            Native::Id3v2(t) => match t.into_iter().nth(i)? {
                frame @ (Frame::Text(_) | Frame::KeyValue(_)) => Some(frame.id_str().to_owned()),
                _ => None,
            },
            Native::Ilst(t) => match t.into_iter().nth(i)?.ident() {
                AtomIdent::Fourcc(code) => Some(code.iter().map(|&b| b as char).collect()),
                AtomIdent::Freeform { .. } => None,
            },
        }
    }

    /// The id of the item a known `key` is written into ([`Native::ident`]), which other
    /// keys may share (`TRCK`, `TIPL`, `trkn`).
    pub(super) fn home(&self, key: &str) -> Option<String> {
        match self {
            Native::Vorbis(_) => None,
            _ => mapped_key(key)?.map_key(self.tag_type()).map(str::to_owned),
        }
    }

    /// Keeps the items `keep` marks.
    pub(super) fn retain(&mut self, keep: &[bool]) {
        let mut i = 0;
        let mut next = || {
            let kept = keep.get(i).copied().unwrap_or(true);
            i += 1;
            kept
        };
        match self {
            Native::Vorbis(t) => {
                let items: Vec<(String, String)> = t.take_items().collect();
                for (k, v) in items {
                    if next() {
                        t.push(k, v);
                    }
                }
            }
            Native::Id3v2(t) => t.retain(|_| next()),
            Native::Ilst(t) => t.retain(|_| next()),
        }
    }

    /// Adds values: `entries` (key, values), each key's under `name` when given (the name
    /// it had: [`ItemName`]), else the library's own name for it in this type of tag.
    pub(super) fn add(
        &mut self,
        entries: &[(String, Vec<String>)],
        name: Option<&(String, ItemName)>,
    ) {
        let name_of = |key: &str| name.filter(|(k, _)| k == key).map(|(_, n)| n);
        // Known keys go through lofty's mapping together, so that keys one item holds (track
        // number and total) end up in that item again; those this type cannot hold by
        // mapping are written under their own name.
        let mut generic = Tag::new(self.tag_type());
        let mut own: Vec<(&str, &[String])> = Vec::new();
        for (key, values) in entries {
            let (Some(item), None) = (mapped_key(key), name_of(key)) else {
                own.push((key, values));
                continue;
            };
            let mut probe = Tag::new(self.tag_type());
            if self.tag_type() == TagType::VorbisComments
                || !probe.push(TagItem::new(item, ItemValue::Text(String::new())))
            {
                own.push((key, values));
                continue;
            }
            // ID3v2: one item, its values joined as version 2.4 joins them (lofty would
            // write a frame per item, and frames such as `COMM` with the same language and
            // description replace each other).
            if self.tag_type() == TagType::Id3v2 {
                generic.push(TagItem::new(item, ItemValue::Text(values.join("\0"))));
                continue;
            }
            for value in values {
                generic.push(TagItem::new(item, ItemValue::Text(value.clone())));
            }
        }
        match self {
            Native::Vorbis(t) => {
                for (key, values) in own {
                    let name = match name_of(key) {
                        Some(ItemName::Vorbis(name)) => name.clone(),
                        _ => mapped_key(key)
                            .and_then(|item| item.map_key(TagType::VorbisComments))
                            .unwrap_or(key)
                            .to_owned(),
                    };
                    for value in values {
                        t.push(name.clone(), value.clone());
                    }
                }
            }
            Native::Id3v2(t) => {
                for frame in Id3v2Tag::from(generic) {
                    t.insert(frame);
                }
                for (key, values) in own {
                    let description = match name_of(key) {
                        Some(ItemName::UserText(d)) => d.clone(),
                        _ => key.to_owned(),
                    };
                    t.insert(Frame::UserText(ExtendedTextFrame::new(
                        TextEncoding::UTF8,
                        description,
                        values.join("\0"),
                    )));
                }
            }
            Native::Ilst(t) => {
                for atom in Ilst::from(generic) {
                    t.insert(atom);
                }
                for (key, values) in own {
                    let ident = match name_of(key) {
                        Some(ItemName::Atom(ident)) => ident.clone(),
                        _ => AtomIdent::Freeform {
                            mean: "com.apple.iTunes".into(),
                            name: key.to_owned().into(),
                        },
                    };
                    let data = values.iter().map(|v| AtomData::UTF8(v.clone())).collect();
                    if let Some(atom) = Atom::from_collection(ident, data) {
                        t.insert(atom);
                    }
                }
            }
        }
    }
}
