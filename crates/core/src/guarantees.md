# Semantic guarantees

API 1.0 is the complete public baseline. Later API minors may only add compatible
capabilities and must preserve existing messages, fields and semantic guarantees.
`Snapshot.features` reports the capabilities actually available in this build/backend;
it is not a list of changes relative to a smaller baseline. Published capability names
are never removed.

<!-- api-1.0-features:start -->
- `multi_artist`
- `cover_edit`
- `playback_limits`
- `output_devices`
- `output_mode_contract`
- `permissions`
- `lyrics`
- `waveform`
- `cue`
- `edit_tags`
- `automatic_loudness`
- `paused_navigation`
- `track_rows`
- `album_paging`
- `search_paging`
- `enqueue_expected_current`
- `playlist_playback_guards`
- `release_root`
- `queue_candidates`
- `structured_signal_path`
- `job_queries`
- `action_details`
- `scan_error_codes`
<!-- api-1.0-features:end -->

The numbered guarantees below are normative. Paragraphs explicitly headed
"Implementation notes (non-normative; may change)" describe current mechanisms and
parameters, not promises about future implementations.

API 1.0 provides structured JSON syntax diagnostics: `details.line` and `details.column`
are 1-based; `details.field` is absent for syntax errors. The message may also contain
position information, but its wording or formatting is not a protocol. Scalar type/value
errors give the request-root field path, including array indices, where it can be
established; otherwise the path may be omitted rather than guessed. Library filter
errors identify the expression, including nested group indices.

**Implementation notes (non-normative; may change).** Failed-request path discovery
currently uses at most 32 probes; syntax message text currently comes from serde.

1. **Ordering per caller.** Commands one caller sends one after another (each waiting
   for its response) take effect in that order. Commands from different callers have
   no defined order relative to each other.
2. **Events are totally ordered by `seq`**, one counter for the whole core; every
   subscriber sees the same numbers. A subscriber may see gaps: events it did not ask
   for (positions when `positions` is off, confirmations for non-UI subscribers) are
   not delivered to it. Lost events are announced by `Resync`, never by a silent gap.
   `Resync` is not a numbered event of its own: its `seq` repeats that of an event
   already emitted (which this subscriber may not have received); what counts is its
   `snapshot.seq` (see 4).
3. **Read your writes.** When a response arrives, the command's effects are visible to
   every query, and every event the command caused has been emitted (they precede the
   response in `seq` order for a subscriber that reads them). A command that starts or
   joins a job (the receipt's `job`: `AddRoot`, `Rescan`, an `UndoAction` that brings a
   folder back; `EditTags` and an `UndoAction` of a tag edit) has done its own
   part by then; what the job does becomes visible to queries with the job's
   `JobFinished`. (These are the commands of this version; later ones may start jobs too,
   and say so.)
4. **Snapshots and `seq`.** `Snapshot.seq` is the `seq` of the last non-`Position`
   event the snapshot reflects. A client that (re)builds its state from a snapshot
   ignores events with `seq` ≤ `snapshot.seq`. `Position` events do not advance it.
   `Resync { snapshot }` means events were dropped for this subscriber (it read too
   slowly): rebuild from its snapshot and refetch anything cached (queue pages,
   library views, the action list). It is sent as soon as there is room, at the latest
   on the next position tick (≤ 250 ms), not only with the next event.
   Trusted Rust hosts can rebuild pending confirmations after a lost event using
   `CoreHandle::pending_confirmations_snapshot(Origin::Ui)` (or the list-only wrapper).
   This is deliberately not a public `Query`: every other origin receives
   `origin_not_allowed`. The actor expires tickets before reading the authoritative
   table; its monotonically increasing read revision orders concurrent recoveries even
   when the event sequence has not changed. Reading never answers a ticket or grants
   permission. Adapters must not expose this API on agent or theme transports.
5. **Origin rule.** Events caused directly by a command, while core handles it, carry
   that command's origin: the `StateChanged`, `Position`, `QueueChanged`,
   `HistoryChanged`, `TracksChanged`, `PlaylistsChanged`, `ActionsChanged` … that
   `Play`, `Pause`, `Stop`, `SetRating` … emit before their response, and the events of
   a held command run on confirmation (the requester's origin). Events driven later by
   the engine, by time or by the system have origin `null`, also when a command led to
   them: the `TrackStarted` (and its `StateChanged` / `Position`) of a `Play`, a track
   ending, an expiring confirmation, a scan started at startup, a change the system makes
   (an output device going away). A job's events carry the origin of the command that
   started the job.
6. **Ids and revisions are never reused** within one data directory, also across
   restarts: `EntryId` and `QueueRevision` only grow (they start from
   `max(saved + 1, Unix ms)`). A stale id gives `not_found`, a stale revision
   `conflict`; neither ever hits another object. Track, playlist, item, root and action
   ids are database ids, never reused either; nor are grant ids, also those of grants for a
   session (which end with core).
7. **Not found, no-ops, ranges.** A query or command naming one object by id fails with
   `not_found` when it does not exist (`Album`, `PlaylistItems`, `PlayEntry`,
   `UndoAction` …); a removed library folder and a playlist in the trash still exist.
   Removing (`Remove`, `RemoveFromPlaylist`) is idempotent: ids already gone are ignored,
   the receipt lists what was removed. A command that changes nothing succeeds with
   `changed: false`: it bumps no revision, records no action and emits no change event.
   These are no-ops: removing only ids already gone; an empty `Enqueue`; a `Move` or
   `MovePlaylistItem` to where the entry already is; `Clear` of an empty queue; `Pause`
   when not playing; `Resume` of an empty queue; setting a setting, a rating or a loved
   flag to its current value; renaming a playlist to its name; `SetSmartQuery` with the
   same query and limit; `AddRoot` of a folder that already is a library folder;
   `RemoveRoot` of a folder removed already; `DeletePlaylist` of a playlist in the trash;
   `RestorePlaylist` of a playlist not in the trash. Non-UI root access requests
   (19) still record their audit and emit `ActionsChanged` when access state is
   unchanged; their `changed` describes the access state, not the audit journal.
   Enum values are checked also when
   the command would change nothing (an unknown `Enqueue.at` is `unknown_variant` even
   with no tracks). Positions and indices past the end are `invalid` / `out_of_range`
   (`Move.to` ≥ length, `AddToPlaylist.index` > length, `Play.start` ≥ context length …).
8. **Pages.** Every paged result has `offset`, `total` and `next_offset`, computed from
   one consistent snapshot. `next_offset` is `null` when the page came back empty or
   reached `total`, so following it always ends. Each paged query has its own page cap,
   stated with its `limit`: in this version pages of full tracks hold at most
   `limits.max_track_page` rows and the other pages at most `limits.max_page`; queries
   added later state their own caps (pages of ids only, for example, may be much
   larger). A larger `limit` is cut, and `next_offset` says where to continue.
   `limit: 0` counts only: no rows, `total`, and `next_offset: null`. Lists of a
   container never drop rows.
9. **Availability.** Browse queries (`Tracks`, `Facet`, `Search`, `Albums`, `Album`,
   `Artists`, `ArtistAlbums`) and smart playlists list available tracks only, unless
   the filter mentions the `available` field. Later versions may leave more tracks out
   of them, such as a file a cue sheet splits into tracks (the sheet's tracks are listed
   instead). Containers (`QueuePage`, `PlaylistItems` of a manual playlist, `History`)
   list everything, with `available: false` on what cannot play.
10. **Positions.** A `Position` event is emitted at once on every jump: seek (also while
    paused or stopped), pause, resume, stop, and a new track. While playing, more
    follow periodically (currently ≤ 4 per second; the rate is not guaranteed):
    clients interpolate with `at_ms` and `rate`.
    With `paused_navigation`, `Next` and `Previous` while paused or stopped
    select the target at position zero with rate zero and keep that state, for every origin.
    `Previous` at or beyond three seconds (the remembered position while stopped)
    restarts the current entry; while stopped this only resets the resume position.
    The new current entry is announced by `StateChanged` (including its snapshot), with
    `QueueChanged` when the queue summary changes and an immediate `Position`.
    Selection alone never emits `TrackStarted` or records a play in history; those
    require actual playback. Paused navigation settles a previously heard entry as
    Skipped; stopped navigation adds no history or play count.
    Resume starts the selected entry normally, from zero or a subsequent Seek target.
    Stopped selection only updates the current entry and resume position: no output
    is opened and no file is probed. Consecutive selections retain only the last one;
    file failures are discovered on Resume through its existing failure path. Shutdown
    and restart restore the selected entry at zero, Stopped. Next without a successor
    stops a paused player; while already stopped it leaves everything unchanged
    (`changed: false`), including the remembered position. An enqueue-only queue has
    no current entry or successor: Next is likewise unchanged and silent; Resume
    starts its first entry. A successful stopped selection reports `changed: true`
    (also a repeat-all restart of the same entry). Playing navigation is unchanged.
    The same feature also covers Stop: it remembers the current entry at
    zero and the Stopped snapshot reports that entry, position zero, and rate zero.
    Resume after Stop starts at the track's beginning (a CUE subtrack's beginning),
    unless a later Seek sets a different resume position. A removed entry retains
    the existing successor rule. Stop still records the Stopped end reason and the
    actual time heard; resetting the resume position does not change played_ms.
    This applies only to the Stop command. Ordinary shutdown, restart recovery,
    PauseClose, Resync, output rebuilds, and natural queue completion keep their
    existing position rules. Exiting without Stop saves the real position; exiting
    after Stop saves zero, unless a subsequent Seek changed it. Repeated Stop while
    already stopped remains a no-op, including after Seek.
    Exception: if Stop's frozen output batch also commits natural queue completion,
    that play ends as Completed and the Stopped position may be null, as at natural
    end of queue; Stop does not invent another play or a zero-position resume anchor.
    Every stopping path retires the old session's playback and output-recovery
    authority, including prefetched instances and delayed starts. Selection while
    stopped also retires any remaining session authority. Historical starts cannot
    replace the media metadata of a stopped selection. Known limitation: Linux/macOS
    navigation buttons remain enabled when the snapshot cannot determine a target.
    A queue populated only by enqueue has no current entry, so Next and Previous
    have no effect even though the buttons may be enabled; Play remains available.
11. **Queue summary.** `QueueChanged { queue }` is emitted whenever the snapshot's
    `queue` summary changes, including only `current_index` (a new track).
    With feature `queue_candidates`, the summary includes `next_entry` (the
    first candidate Next tries) and `natural_next_entry` (the first candidate at natural
    completion). Candidates follow core's current shuffle order, never display order.
    Repeat off ends at the tail; repeat all wraps; repeat one holds only natural
    completion, while Next advances without wrapping. A removed current entry uses
    its remembered first surviving successor for both candidates, even with repeat
    one/all; removing a tail or later removing that successor does not restart at the
    head or choose another successor. An empty or enqueue-only unselected queue has
    null candidates. Paused/stopped selection retains these navigation candidates;
    natural completion is hypothetical while not playing. Stopped Next selects only,
    whereas Resume can start the current entry or queue head. Candidates do not probe
    files or skip unavailable tracks, so failure handling and concurrent changes can
    change the actual result. EntryId distinguishes duplicate tracks.
    Candidate changes emit QueueChanged, including after repeat/shuffle changes,
    explicit selection, edits, undo and natural advancement. Queue revision protects
    edits only: events with the same revision must still be applied. Subscribe from
    a snapshot (and replace it on Resync), then merge QueueChanged.queue.
12. **Who may do what.** Each command has a risk tier (`x-risk`: `control`, `write`,
    `destructive`, and `file_write` for rewriting the user's files in a way
    that is recorded and can be undone, `EditTags`; more tiers may come) and
    a **default** policy per origin (`x-origins`: `allow`, `confirm`, `forbidden`). The
    user may grant an agent more or restrict it (by command or class of commands,
    folder and time; docs/design/agent-permissions.md), and core may require more than
    the default for some arguments. The user's grants (`GrantPermission`,
    `RevokePermission`; UI only, like everything about agents and tokens) apply to one
    agent (and session, if the grant names one: a session that has ended cannot be granted
    anything, `invalid` / `session_ended`), never to other origins: a default of
    `forbidden` stays; then a matching `forbid` refuses the command (`forbidden` /
    `permission_denied`, naming each such grant and its operation in
    `details.forbidden_by`, and whether it covers all of what the command acts on or a part,
    never its scope: where the user forbids an agent is not the agent's to learn), also one
    the default allows, and wins over any `allow`; then a
    default of `allow` runs it; a command the default asks about runs without asking when
    a matching `allow` covers all it acts on, and is asked about as before otherwise. A
    `forbid` matches what its scope may contain, and a whole folder that holds its scope
    (adding or removing a music folder, or undoing that, acts on everything inside it); an
    `allow` must contain all of it. A grant never answers a confirmation already pending:
    an `allow` given meanwhile does not run it, but a `forbid` given meanwhile refuses it
    when the user approves it (`permission_denied`). An undo is judged by what the undo
    itself does:
    undoing a folder change changes a folder (it is treated like `AddRoot` /
    `RemoveRoot`: undoing a removal is adding the folder, undoing an addition removing it,
    and grants for those commands apply), undoing a tag edit rewrites files (it is treated
    like `EditTags`: class `file_write`, agents and remotes are asked, and grants for
    `edit_tags` apply); undoing a change to library data (ratings, playlists, also undoing an import, which
    moves the playlist to the trash) is a library data change. Clients must therefore handle
    `needs_confirmation` and `forbidden` for any command. Permanently deleting library
    data (`PurgePlaylists`) is never granted: from an agent it needs the user's
    confirmation every time, unless the user forbade it. (Writing a file, `ExportM3u` or
    `EditTags`, is a different class, `file_write`: the user may grant it. A tag edit acts
    on the tracks' files, each where it really is (a file a link in its library folder
    leads out of it is shown, and scoped in `PendingConfirmation.scope`, where it really
    is); for an agent, a file that no longer is where it was checked when it is written is
    not written: `location_changed` in the job's report. A file whose location cannot be
    read now (a link to nothing, a drive that is not there) is skipped, and a confirmation
    says so; a tag edit none of whose files can be read, or the undo of one with such a
    file, cannot run: `invalid` / `invalid_location`.) A location an agent names must name a
    place by its text alone (desktop: absolute, with no `..` part and no control or
    text-direction character): one that does not is `invalid` / `invalid_location` before
    any grant is looked at. It is taken in
    the platform layer's normal form (16), and the grants and the user see that form; one
    the platform layer cannot read now (a link to nothing, a drive that is not there) is
    `invalid` / `invalid_location`, unless a `forbid` contains it where it is by its name
    (then `permission_denied`); one that does not answer in time is `invalid_location`
    before any grant is looked at. A music folder whose location cannot be read now is
    judged by its name the same way (removing it, or bringing it back, changes only the
    library). When the user approves,
    core works it out again, and the library checks it once more right before it reads or
    writes there: a location that no longer leads where it did (a symlink put in its way
    meanwhile) is `invalid` / `location_changed`, and nothing is done (a location where
    nothing exists at all fails as it would for anyone: nothing changed). Known limit: a
    music folder is checked when it is added; later scans read it at its path, so a folder
    an agent added and then replaced by a symlink is scanned where the link leads (that
    exposes the paths and tags of audio files there). An agent's
    `ImportM3u` lists in `missing` only the lines that look like media (a path or URL with
    an audio or cue sheet extension); `missing_count` counts them all. A
    command that needs the user's confirmation fails with
    `needs_confirmation` and `details.confirmation`; only the UI sees
    `ConfirmationRequested` and may answer with `Confirm` / `Reject`; core then runs the
    command itself. Do not resend it. Every subscriber sees how it ended in
    `ConfirmationResolved` (`outcome.reason`: `approved`, `rejected`, `expired`,
    or `withdrawn` when the requester's session ended or its agent was removed first; an
    approved command whose session ends or whose agent is removed or registered again
    before execution starts is withdrawn too, including during its approval recheck).
    Commands already executing (including started tag jobs) finish normally. Registration
    instances are bound at connection authentication (or on the calling thread before a
    direct agent request is queued) and never identified by wall-clock time. Session
    invalidation takes effect before adapter cleanup waits for blocking workers. Every
    agent command is checked once more immediately before execution, including after
    synchronous policy reads. Ending a session or removing an agent immediately withdraws
    pending confirmations and approval rechecks on the actor, releasing the original command's key and
    answering a pending approval without waiting for its worker. Core checks what it cheaply can (unknown ids,
    paths, file names, an M3U file to import that is no regular file, a folder to export
    into that does not exist, an undo undone already) before asking, so the user is not asked to
    approve a command that cannot run or that it knows would change nothing; it checks
    the user's `forbid` grants before any of that.
13. **Idempotency.** A request with an `idempotency_key` is executed at most once per
    origin, registration instance and key within 10 minutes (the origin's kind and name;
    an agent's `session` is not part of it, so a retry after reconnecting to the same
    registration is still recognised; a new registration of the same name has separate
    keys) (at least the last 1024 unexpired keys are kept): a retry returns the
    first result without running the command again (a pending confirmation's retry
    returns the same token; once answered, the command's outcome). Results of kind
    `failed` are not kept, so a retry with the same key runs the command again; nor are
    those of `RegisterAgent` and `RenewAgentToken`, whose token is shown once (a retry
    registers again, `already_exists`, or hands out another token; the key still belongs
    to the command); nor are withdrawals (`confirmation_rejected` because the requester's
    session ended or its agent was removed before the command ran or was answered): the
    command never ran, so a retry runs it, and is checked and asked about as any. Keys
    live in memory
    only: they do not survive a restart of core.
14. **Threads.** `execute` returns when the command is done (see 3). Work may move off
    core's command thread in later versions; 1–3 hold regardless.
15. **Actions and undo.** These changes are recorded as actions: ratings and loved
    flags; playlist edits (create, import, rename, add, remove, move, set query, move to
    the trash, restore); adding and removing library folders; tag edits (one
    `tag_edit` action for the files a job wrote, none when it wrote none; ratings are
    never written to files); undos themselves. Later versions may record more kinds
    (`ActionKind` is open). Not recorded: the queue (it
    has `Undo`), settings, scans, and what cannot be undone (`PurgePlaylists`,
    `ExportM3u`, which writes a file); for those, an audit entry is kept instead, when they
    permanently delete playlists (`purge`), request non-UI root access changes
    (`root_release`, `root_resume`; see 19), or write a file (`file_write`: when it replaced
    one, and every file an agent wrote): listed by `Actions` like an action, never
    `undoable` (`UndoAction` of one is `not_found` / `action_not_found`), announced by
    `ActionsChanged`. The receipt's `action` names the recorded action, so it can be
    undone, never an audit entry. The optional receipt `audit` separately names an
    irreversible operation's audit entry for attribution; it does not make it undoable.
    For a change a job makes (a tag edit, or its undo), the
    job's report names it instead, when the job finishes. An action an agent ran because of a
    grant names it (`grant`). `Actions` lists the
    journal (actions: at least the newest 10 000 within the last 30 days;
    a playlist keeps its last 50 undoable edits; audit entries apart: each
    is kept for a year, and at least the newest 10 000 however old)
    and `ActionsChanged` announces new actions and undos. The playlist clause retains
    this wording: it describes undo depth within the surviving global journal,
    not a separate archive exempt from its time and count eviction. The per-playlist
    cap removes older inverse payloads; it does not protect the newest 50 edits from
    global journal eviction. Currently actions older than 30 days are no longer
    undoable even before pruning, and global pruning removes expired actions and
    entries beyond the newest 10 000. Consequently fewer than 50 playlist edits may
    remain undoable; an independent, time-unlimited minimum of 50 is not promised.
    `UndoAction` undoes one
    action by id; the undo is itself an action, so undoing it redoes the change. An undo
    never overwrites a later change: if another action changed one of the same objects
    since, it fails with `conflict` / `action_conflict` and `details.changed` lists those
    objects. Undo and redo of a newly recorded tag edit restore the corresponding
    complete file byte for byte. During batch preflight, a missing file or changed stamp
    fails the entire undo with `action_conflict`. A digest mismatch with an unchanged stamp
    reports `file_changed` before any file is written. A conflict or I/O failure found after
    publication has started can leave completed files in the same batch. They are recorded
    separately from the remaining targets.
    Development actions recorded without a whole-file inverse retain their legacy tag
    restoration behavior. Their undo action summary identifies this behavior. These entries
    check size and modification time, and also check the protected ID3 bytes or pictures
    when their record contains a protection digest.
    A tag edit refuses an externally changed target with `file_changed`.
    Trusted hosts can describe an undo through the Rust API before routing it:
    `describe_undo` validates the entire retained undo chain, decodes the effective
    inverse, checks target states, and returns the original action, effective operation,
    whether it writes files, its actual targets and a fingerprint. Unknown, corrupt,
    pruned, already-undone or conflicting actions fail closed. `execute_described_undo`
    accepts only `Ui` and the described action. It checks the description again on the
    actor; data-only undos check inside the writer transaction, file undos after acquiring
    the serialized tag-job gate. Partial file undos change the remaining targets and
    fingerprint, so an older description cannot authorize a later remainder. Ordinary
    execution/idempotency and agent policies remain unchanged.
    A tag preview (including the Rust undo preview) can show the canonical display
    location resolved by the library. The preview can create and remove disposable files
    to check directory permissions and volume capabilities. The actual write rechecks
    location and file availability. Use the optional tag-job report `written` to identify
    physical writes.
    `written_count` reports completed targets and can exceed `written.len()` when a
    legacy undo completes with the requested values already present, or when a subsequent
    redo completes without a physical write.
    Permanent-deletion confirmations bind concrete playlist IDs at request time,
    including `PurgePlaylists { playlists: null }`. Their summaries name those playlists
    and track counts. The library compares the frozen selection and display data inside
    the deletion transaction; a changed all-trash set, restored/missing playlist or
    changed selected metadata refuses the entire operation and requires a new review.
    Trash has no automatic deletion deadline: startup, ordinary deletion and other
    commands never permanently delete expired playlists. Items older than 30 days
    remain restorable. The trusted window marks them and offers manual cleanup through
    the same permanent-deletion confirmation with the exact count and a review sample;
    it selects only expired IDs and freezes the entire trash snapshot. Any trash
    change during that review refuses execution, including inside the writer transaction.
    Host preflight and the deletion transaction each recompute the complete expired
    set at their current wall time and require it to equal the reviewed set. Natural
    threshold crossings, lost eligibility or inconsistent clock observations require a
    new review. Approval uses a monotonic deadline so changing wall time cannot alter
    the required wait.
    Every successful nonempty purge writes an audit entry. No grant can allow purge.
    A guarded or unguarded purge deletes only its selection, with no incidental expiry.
    Trusted hosts can use `describe_purge` / `execute_described_purge` for
    the same atomic binding; execution accepts only `Ui`, with either explicit IDs that
    match the binding or `playlists: null` when the binding covers the whole trash
    (`all`), which the transaction then re-reads in full.
    These Rust descriptions confer no permission and are not exposed through Query.
    Pending-confirmation recovery returns requests in creation order, including ties
    in expiration time; it never orders requests by the lexical form of their tickets.
    Trusted approval requires at least an 800 ms wait and a review sample of at least
    the first 20 items (or every item when fewer exist).
    **Implementation notes (non-normative; may change).** Clock disagreement is detected
    with a 20 ms precision allowance plus monotonic elapsed time times 1000 ppm, rounded
    upwards to milliseconds. Normal gradual correction is allowed; reviewed-set equality
    is still exact. The current journal action capacity is 10 000 entries.

16. **Opaque strings.** Location strings (`AddRoot.path`, `RootInfo.path`,
    `ImportM3u.path`, `ExportM3u.path`) are defined by the platform layer: absolute paths
    on desktop, platform locations such as folder URIs on mobile. Clients display them or
    pass them back, and never build file paths from them: `TrackInfo.path` is a
    root-relative location for display and sorting. Origin names (`agent.name`,
    `remote.device`, an agent's `session`) are identifiers the adapter chooses: a
    registered agent's name has no `:`, an agent an adapter names itself is
    `<platform>:<name>`, and a session is unique and never reused (random).
    Core stores and compares these strings but does not interpret them otherwise; the
    platform layer may normalise a location (a desktop folder is stored in its canonical
    form, so `RootInfo.path` can differ from the `AddRoot.path` sent; a location an agent
    names, or a grant's, is taken in normal form: symlinks and `..` resolved as the
    system resolves them; an agent's may not have a `..` part).
    An agent command path is normalized before grant checks and preview, then checked
    again before execution; control characters are not allowed. A changed target after
    review returns `invalid/location_changed` and performs no write.
    In contrast, every `GrantPermission` location scope, including session grants,
    must already equal the reviewed platform-normal form. Core does not silently
    normalize a submitted grant into another scope: a different normal form returns
    `invalid/location_not_normal`, field `command.scope.location`, and creates no grant.
    Normal forms come only from values supplied by core or the host platform layer;
    clients must not construct or guess them. An unusable location is `invalid_location`.

17. **Atomic playback guards.** `Enqueue.expected_current`, when present,
    requires `at: next` and the same current queue EntryId at insertion time. A track
    change does not bump the queue revision, so clients batching next insertions should
    guard both the current entry and the revision. A changed, removed or absent current
    entry returns `conflict` / `entry_not_current`, even for an empty batch. Using the
    anchor with another insertion position returns `invalid` / `invalid_request`.
    A playlist `PlayContext` may carry `start_item` and `expected_revision`. Revision
    checking, exact item lookup and expansion of the playable queue share one catalog
    snapshot. The item selector takes precedence over `Play.start_track` and `Play.start`;
    the entire bounded context is retained, including the prefix before the item.
    A stale revision or an absent, foreign, unavailable or context-limit-excluded item
    returns `conflict` / `playlist_conflict`. An item selector on a smart playlist is
    `invalid` / `playlist_kind_mismatch`. A smart playlist revision guards its definition,
    not changes to the library rows matching its query. Rejection changes neither
    playback nor queue entries, revision or undo. Omitted or null guards preserve the
    earlier behavior; successful earlier batches are not rolled back.

17. **Browsing and argument features.** `album_paging` adds `Album.offset`
    and `limit`. Omitting both preserves API 1.0: every available album track is
    returned, including albums larger than `max_track_page`. Supplying either opts
    into paging (offset defaults to 0, limit defaults to `max_track_page` and is
    capped there). Host byte budgets still apply: large results use chunk transport;
    results above the host read budget return `response_too_large`, so the client
    must use explicit pages to retrieve the complete album. `album.total`
    equals `album.summary.track_count`; `offset` and `next_offset` describe its track
    page. Available tracks use disc number (missing means 1), track number (missing
    last), relative path and finally track ID. `limit: 0` still returns the summary
    and total. Playback contexts continue to resolve the complete album.
    `search_paging` adds optional `Search.kind` (`tracks`, `albums`, `artists`) and
    `offset` (default 0). A selected kind returns only its list, `total`, `offset`
    and `next_offset`; the three `*_total` fields still count all matching kinds.
    Omitted/null kind preserves mixed results. Its offset must be 0; nonzero offsets
    return `invalid` / `out_of_range` with field `query.offset`. Select a kind to page.
    Each mixed list is capped by `limit`, with `next_offset: null` and `total` the
    sum of the three totals. Relevance order is
    stable: tracks use title matches first, then FTS score (short-text fallback:
    normalized title), then track ID; albums use title matches, normalized title,
    then album key; artists use descending track count, then normalized artist name
    (their key). Stable order prevents duplicate/omitted rows while the library is
    unchanged; it does not freeze different requests against concurrent edits.
    `enqueue_expected_current` advertises `Enqueue.expected_current`;
    `playlist_playback_guards` advertises both playlist context `start_item` and
    `expected_revision`. Clients must check the available features before sending
    these fields; unavailable capabilities do not authorize an operation.
    The host accepts direct results up to at least 2 MiB and larger results through
    chunk transport, with a read-cache byte budget of at least 32 MiB (see 23).

18. **Automatic loudness only.** Manual `ScanLoudness`, `ClearLoudness` and
    `CancelLoudnessScan` existed only during unfrozen development and are
    absent from the API 1.0 baseline; their wire names are
    unknown commands (`unsupported` / `unknown_command`). No manual job ownership or
    cancellation interface remains. `automatic_loudness` and its background work,
    cached `Loudness` query, `TracksChanged`, and automatic `loudness_scan` job
    progress/report events remain. The old `loudness_scan` feature is not advertised.
19. **Released library folders.** `release_root` advertises `ReleaseRoot` and
    `ResumeRoot`. Non-UI requests that pass authorization and active-root checks
    persist a non-undoable `root_release` / `root_resume` audit entry before changing
    access, with origin and the authorizing grant. Retries and already-suspended or
    already-resumed requests are audited too. `ActionsChanged` announces the entry;
    successful responses return its ID in `receipt.audit`, never `receipt.action`.
    The audit records the request, not proof of barrier completion: a later I/O error
    or timeout can leave suspension in place. UI requests do not add these entries.
    They follow the journal's audit retention rules. A successful release response is a file-access barrier for one registered
    folder: if current playback or preloading uses it, the whole transport stops
    (including when paused), both decoders close, and no successor starts. Playback
    wholly in other folders continues. Queue entries/order/revision, playlists,
    indexed availability, tags and history are retained; a heard play ends normally
    as stopped. Release waits for in-flight library scans, tag reads/writes, covers,
    lyrics, loudness and waveforms to close their files. Every playback open acquires
    root-counted leases before invoking the resolver and retains them through decoder
    destruction, including all canceled/replaced prefetch tasks that outlive their
    session or queue candidate. Release refuses new file access
    with `root_released` (tag edits report it as a skip; tag undo fails before any
    write). Trusted adapters synchronously unwatch native directory handles through
    core's Rust-only root-access registration; watch/rebuild paths share the same
    suspension lock and cannot reopen them. Failed native closure returns retryable
    `failed` / `io` and leaves the root suspended: retry release to prove closure,
    or explicitly resume. `Roots.released` records suspension, not successful closure. Existing operations may finish while release is pending, so the caller
    must wait for a successful response before removing media. A timeout or lost
    response is not proof of release.
    The actor marks suspension, freezes output at its final rendered boundary and
    revokes the affected session's authority without joining its decode thread. Late
    events are isolated from the next session. Background work joins retired main
    decode threads and drains all root leases using **one total 5-second deadline**,
    measured from suspension; joining does not get an additional lease-drain budget.
    The idle/finished predicates are checked once even at the deadline boundary.
    Native participant notification then has its own **5-second deadline**. A native
    timeout is retryable `failed` / `io` and retains both suspension and the original
    notification identity. Release, resume, removal and directory undo return
    `failed` / `job_busy` until that callback actually exits; a retry can then consume
    its result, or ResumeRoot can notify released=false before lifting suspension.
    Thus late released=true cannot overtake a subsequent resume. A participant panic
    or drain-worker panic returns an error rather than stranding a caller. Finished
    retired threads are reaped without blocking on still-running ones, including
    after timeout, resume or removal. This deadline promise applies to ReleaseRoot;
    ordinary Stop and track switching can still synchronously join decode threads.
    A timed-out thread and its source stay owned until actual exit, and retries include
    that same retirement. Other commands, engine/job events and Shutdown remain
    serviceable. Timeout returns retryable
    `failed` / `io`, keeps suspension, and permits a release retry. Release/resume
    of the same root while its barrier is pending returns retryable `failed` /
    `job_busy`. Confirmation resolution and
    idempotent success are also deferred until the barrier completes. Shutdown drops
    pending barrier answers (`core_stopped`), does not join drain workers, and ignores
    late completion; existing scan/tag jobs retain their usual shutdown behavior.
    Scans acquire leases one root at a time and check release at file, CUE and batch
    boundaries. A released root is abandoned promptly after its current I/O returns,
    without marking missing tracks or finishing that root. Scans, including requests
    from a watcher/periodic host, skip released folders
    without marking their indexed tracks missing. Automatic analysis skips them.
    Merely querying, playing, adding an already active root again or requesting a rescan does
    not resume them. `ResumeRoot` explicitly permits access and schedules a scan and
    automatic analysis, without starting playback. Suspension is shared by the
    library instance for this session, not persisted. Successful RemoveRoot clears
    suspension and notifies trusted adapters with released=false, so re-adding that
    path or undoing the removal revives an enabled root. This same transition applies
    to removals produced by ordinary/guarded UndoAction and redo (including undoing
    AddRoot); all root lifecycle undos check pending barriers before committing and
    return retryable `failed` / `job_busy`. Repeating AddRoot for an active root never
    clears suspension. Removal changes and action invalidations are published even
    when participant recovery fails after commit: `failed` / `io`, retryable, means
    the removal is committed but adapter recovery is incomplete, not rolled back.
    Core retains that notification and a repeated RemoveRoot retries it; after recovery
    this retry succeeds with changed=false and no new action. For an already committed
    directory UndoAction/redo whose notifications fail, the error is instead
    non-retryable (`details.retryable=false`): do not repeat UndoAction. Its
    `details.recovery_command="remove_root"` directs the caller to RemoveRoot for
    every removed root of that action. Recovery records for all roots exist before
    the first notification. If reading roots after the undo commit fails, all affected
    root suspensions are conservatively cleared; the committed action is not undone. The root stays absent
    from Roots, and Actions can be queried to find the committed removal action.
    RemoveRoot while a release is pending is retryable `failed` / `job_busy`.
    Reopening the data directory
    restores access. No operating-system unmount/eject is attempted. Success concerns
    core's access to this folder, not other roots/apps or a library database/cache
    placed on the same drive.
    Both commands return `receipt.root` and `changed`; repeated release/resume is a
    no-op after the prior barrier has completed. Release returns no job/action; resume may return its scan job. Unknown or
    removed roots give `not_found` / `root_not_found`. Changes emit `LibraryChanged`;
    `Roots.released` reports the session state. These are `risk: write`, class
    `local_path`, targeting the entire library root: an Agent needs confirmation by
    default, scoped allow/forbid grants apply, Remote is `path_not_allowed`, and
    MediaControls is `origin_not_allowed`. This follows folder authority rules
    because suspending a folder changes local access and can stop playback; it is
    not a playback-only control or a file rewrite. Non-UI requests create the
    irreversible root_release/root_resume audit described above; no undoable action is created.
    **Implementation notes (non-normative; may change).** The scheduler currently
    permits at most 16 concurrent root barriers.

20. **Tag files in use.** Only files actually held by engine decoder/open-task
    leases are in use, including playback, preloading and paused decoders that
    remain open. A retained current selection while stopped, or paused without an
    open decoder, is not in use. Failed or cancelled prefetch tasks remain in use
    until their sources close. These files are skipped with `file_in_use`. An accepted edit may therefore finish with zero written
    files. Core never queues these edits for later writing and stopping playback
    does not apply them. Clients may display “retry later” and must submit a new
    explicit edit after the file is released. CUE selections retain their `cue_track` skip; tag undo retains its existing
    all-or-none in-use failure.

21. **Structured signal path (`structured_signal_path`).** English stage
    name/detail and bit-perfect reason/reasons remain display text, not a parsing
    protocol. StageInfo.kind and SignalPathInfo.reason_codes are stable identifiers;
    unknown enum values decode to unknown. Reason codes describe the verdict as a
    collection, not a positional mapping to English strings. Yes has no causes;
    Uncertain shared output has shared_mixer; No lists known sample alterations or
    a lossy source. An added gain cause replaces the shared-mixer uncertainty, as
    before. ReplayGain gain_db is the final applied dB after preamp and peak protection,
    gain_scope is the actual selected track/album pair, and gain_source distinguishes
    tags, automatic measurement and fallback preamp. Missing provenance from an older
    Catalog remains unknown. Gain/peak provenance is read with the pair, not inferred
    by comparing values. Zero dB retains its source and does not alter samples; Off
    has no RG stage. Volume reports its applied dB, or null with silent:true at zero
    or mute; no infinities are sent. Format stages carry numeric source and device
    destination formats for the overall conversion, not an intermediate DSP buffer
    representation. Unavailable values and inapplicable stage parameters are null.
    Signal paths remain null while stopped; settings/measurement updates publish the
    current path through StateChanged and new playback through TrackStarted.

22. **Desktop host job cache (API 1.0 host cache).** A read-only,
    whitelisted host extension returns recent job progress, final file reports,
    approved-preview skips, cache generation, observed event sequence and a sticky
    incomplete flag. It exposes no pending confirmations or approval credentials.
    Subscribe before reading and merge by job/type/sequence: cache reads can race live
    events; never discard a newer event because it is absent from the cache. Paged
    reads use an immutable connection-bound snapshot with stable generation/sequence/
    completeness. Expiry and eviction fail explicitly. At least the most recent 64
    reports and 64 previews are retained subject to a shared byte budget of at least
    4 MiB. Eviction, stripped review metadata or subscription Resync marks the cache incomplete. Theme reload
    retains the cache; process restart retention is not promised. This is neither
    authoritative active-job discovery nor durable history.

    The first page copies the cache under its lock, then projects/serializes outside
    it. Query snapshots share a cache with capacity for at least eight entries and
    32 MiB, and a lifetime of at least 60 seconds unless explicitly evicted (see 23).

23. **Desktop read budgets.** Input, query/read-cache and event-delivery budgets are
    independent. Large projected results are chunked on a connection-bound immutable
    snapshot; expiry/eviction fail explicitly. Oversized results return structured
    `response_too_large/completed`, retryable false, with byte counts. Clients can ask
    for smaller pages or track batches; tag previews are never silently truncated.
    Commands do not enter this query-size failure path after execution. Event overflow
    requires Resync; an oversized recovery snapshot reports a structured stream error
    rather than truncating it. Chunk reads use an independent lane and do not starve
    normal queries. These are host guarantees, not changes to core v1.

    The host accepts at least 2 MiB of UTF-8 JSON input, including port-envelope
    escaping. Read chunks carry up to 64 KiB of UTF-8. Read snapshots have capacity
    for at least eight entries and a byte budget of at least 32 MiB, with a lifetime
    of at least 60 seconds unless explicitly evicted. Event queues have capacity for
    at least 256 frames and 4 MiB of serialized data. The independent query.read /
    jobs.read lane permits at least 256 calls/second and eight in-flight reads;
    ordinary queries permit at least 120 calls/second. SDK chunk reads have their
    own eight request slots, independent of ordinary queries.

    **Implementation notes (non-normative; may change within the normative floors).**
    The current input limit is 2 MiB; read-cache capacity is eight entries / 32 MiB
    with 60-second expiry. Event capacity is 256 frames / 4 MiB; read quotas are
    256 calls/second and eight concurrent reads, ordinary queries 120 calls/second.
    The retained host cache currently caps reports and previews at 64 each / 4 MiB.

24. **Desktop host pages and close policy.** Fixed history, settings and read-only
    diagnostics destinations supplement host.open; recovery retains its existing
    recovery side effects. Diagnostics show the app build, actual runtime WebView
    version, platform, problems and cache completeness. Windows/Linux default to
    exit; the trusted host settings page can persist close-to-tray separately from
    theme preferences. Native close and SDK request_close share the same path;
    unavailable trays never cause a Windows/Linux window to hide. macOS preserves
    its existing close behavior. Fixed native recovery keys are global on Windows
    and application-scoped on macOS/Linux. macOS consumes Cmd+Shift+F12 through an
    NSEvent local monitor before NSWindow/WKWebView dispatch; Linux retains GTK's
    locked accelerator group. Fresh Agent confirmations received while the theme is
    hidden to the tray show the trusted window without an explicit focus request
    (native no-activation on Windows/macOS, GTK no-focus-on-map hint on Linux).
    Presentation never approves a command; registration failure is logged and the
    tray entry remains. The unused resources.asset and public start_dragging SDK
    interfaces were withdrawn by author decision before any theme depended on
    them; package resource serving, bridge drag fallback and Query.Loudness remain.

25. **Authoritative job reads.** Job and Jobs are read-only for all origins,
    including agents and themes; they never start, retry, cancel, approve or acknowledge
    work. Started jobs are visible before their receipts. Tag terminal state is recorded
    before TracksChanged, ActionsChanged and JobFinished; other progress/outcomes are
    recorded before their corresponding events. Queries can lead event consumption.
    Job returns a full terminal report or failure. Jobs returns only JobSummary rows:
    id, kind, state, optional progress, terminal JobCounts or error {kind, code}, never an outcome,
    failure message or diagnostic path. Completed jobs with observed progress set done
    equal to total; unknown totals become the final observed done. Without any observed
    progress the field stays absent. Cancelled/failed jobs keep their last observed value.
    All running jobs are retained as small metadata. At least the newest 1000 terminal
    jobs are retained subject to a byte budget of at least 32 MiB, evicting the earliest
    finished first.
    Automatic loudness shares retention with scans and tag jobs. There is no time-based
    expiry or persistence; restart clears history. A single over-budget report is not
    retained (or may retain only a summary); it must not evict unrelated job history
    or be silently truncated. Lists contain no full file-result arrays. Unknown/evicted
    IDs are not proof that work never ran.
    Joined scans share a job ID and cumulative report. Scan `JobProgress.done` counts
    processed supported audio paths and `total` estimates audio paths found by the walk;
    each deduplicates `(RootId, relative_path)` within the job across all passes.
    Unchanged files and malformed audio count, but directories, CUE text and other
    non-audio files do not; CUE virtual tracks do not multiply the underlying audio
    path. This measures neither decodes nor the file snapshot at completion: a path
    visited then deleted still counts, and deduplication never skips a required pass.
    Only progress uses this distinct-path correction. Final `JobReport::Scan`
    added/updated/restored/removed counts retain their existing accumulation of applied
    changes across passes; its error count and sample retain one error per path within
    the job. Unknown/evicted IDs return
    not_found/job_not_found. IDs are unique within a run and normally across runs, but
    clock rollback can reuse an ID; clients must not save JobIds across core runs.
    Treat every reconnection as a new run and discard saved JobIds; there is no current
    run_id handshake. Snapshot.run_id may be added compatibly in a later minor.
    Lists use descending ID, offset default 0, max_page cap, limit 0 counts only, and
    offset/next_offset pagination. Pages are internally consistent, but starts/evictions
    can shift offsets: deduplicate by ID. A missing job never proves no work ran.
    The host's bounded event cache is separate from this authoritative history.
    Trusted desktop adapters may run bounded directory repair attempts after a scan
    reports file errors. These are internal maintenance, not public commands or jobs
    while running: their progress and unchanged failures create no Job/Jobs rows or
    job events. The original scan's terminal report remains readable under normal
    retention. A repair with changed error diagnostics or applied library changes
    publishes a new terminal scan job/report; new errors are never silently discarded.
    An ordinary scan request joining a repair promotes it to an ordinary visible job
    before returning its receipt, with normal progress and completion semantics.
    Repairs preserve missing/out-of-scope rows; ordinary scans verify removals.
    Periodic full verification also uses internal completion: unchanged error sets
    and no applied library changes create no new visible job/progress/report events
    or Job/Jobs rows. The previous report remains available under normal retention;
    changed errors (including recovery) or library changes publish a terminal scan
    report. Explicit user scans and other ordinary scans keep normal job semantics,
    including promotion when joining maintenance before their receipt is returned.
    Maintenance compares the complete errors in the actually covered scope, including
    errors beyond the public sample; errors outside that scope remain unaffected.
    If exact facts are unavailable, publish conservatively rather than hiding changed
    failures. No complete public error enumeration is promised. JobReport::Scan.first_errors
    contains at most 20 items, as specified by its field documentation.
    Resolved CUE-to-audio dependencies are recorded before audio reads, including
    first imports with no CUE rows. Audio-directory repairs include their sidecars and
    direct dependencies, so claimed audio is actually rechecked and recovered as CUE
    tracks, rather than falsely succeeding or importing a standalone track.

    **Implementation notes (non-normative; may change).** Retained history currently
    has a 1000-terminal-entry cap and a 32 MiB budget. Each encoded report is charged
    its bytes plus 512 B metadata; running metadata and exact scan diagnostics also
    consume the budget. In-flight query snapshots are not charged to retained history.
    Reads currently take an Arc under a mutex and decode outside the lock. These
    choices bound retained data, not process RSS.
    Retries use only reported failure directories beneath active roots, never an
    unscoped Rescan. Unchanged error paths/causes/messages and file size/mtime stop
    further quick retries. Periodic verification still runs normally and retains this
    signature; file changes or a newly observed error set can permit bounded retries.
    Full verification and scoped repairs share one complete failure signature,
    updated only in the actual covered scope and independent of public sample
    membership/order; unvisited errors and their file versions are retained.
    Root availability is handled by reconciliation rather than file-error retries.
    The bounded public error sample selects quick-retry directories, but it is never
    used to estimate a directory's error count. Rust-only scan facts retain a digest
    for every failed path and the actual covered directories/dependency files; repair
    visibility compares errors in that same scope, including failures beyond the
    public sample. These facts share the current 32 MiB history budget and are evicted
    with their job.

26. **Operation-time action facts.** Optional `ActionSummary.details`
    distinguishes tag edits (changed keys and set/remove operations, with no tag
    values), roots (ID and path), rating/loved edits (new value, including cleared
    rating), playlists (ID and name after the operation), and undo/redo (original
    action kind, following the undo chain). Facts are recorded in the same database
    transaction as the action and persist with its normal journal retention, even
    when undo payloads expire. Unrecoverable original kinds and unknown stored detail
    types are omitted, never re-emitted as decode-only unknown. They are not reconstructed from current objects or
    English summary text. Old journal rows omit details. Tag changes are the union of keys whose primary values changed in a successfully
    written file. They exclude unchanged and skipped keys and never include values.
    An encoding-only conversion can have an empty changes list; its conversion is
    identified in the job report. The list does not assert that every file changed
    every listed key.
    Names describe fact families for root/playlist, retained operation names for
    tag_edit/set_rating/set_loved, and the undo family for both undo and redo. The
    enclosing ActionKind distinguishes operations within a family; new variants should
    reuse a family when its facts are the same, and use an operation name for distinct
    operation-specific facts. TagKeyChange is {type: set|remove, key}, matching TagChange
    without values. Root-release/resume audits carry Root facts just like root changes.
    These read-only facts confer no permission to repeat or undo an operation.
27. **Structured scan causes.** Optional `ScanErrorInfo.code` is an open
    scan-specific enum with an unknown fallback. Causes are assigned at the failure
    site, never inferred from message text. Existing path and message remain. Codes
    distinguish unreachable roots, existing folders with no audio files, directory
    reads (directory_read_failed), invalid paths, file attributes (stat_failed; not tag metadata),
    audio reads, CUE reads, reader panics, and CUE references to missing files. These
    are diagnostics, not permission decisions or an instruction to retry or remove data.

    Ordinary and CUE-referenced audio reader panics both use reader_panicked; other
    audio read failures use audio_read_failed. An existing folder with no audio files
    where indexed files existed uses no_audio_files rather than root_unreachable.
    Full scans, including periodic verification, treat unreadable paths as unknown:
    enumeration or stat failures preserve existing availability for that path and
    all its descendants. A root enumeration failure (even after some files were
    found), or an error without a usable relative path, disables missing-file
    decisions for that root. Successfully observed files can still be updated.
    Transient stat failures therefore do not remove and later restore indexed tracks;
    a subsequent readable scan can establish an actual disappearance.
    Only currently available CUE cuts preserve audio ownership on an unknown read.
    Historical unavailable cuts do not hide an available whole-file track again;
    a currently readable, valid CUE sheet can establish new ownership normally.

    Scan error messages are at most 1024 UTF-8 bytes. Missing-file CUE diagnostics show
    at most ten names, each at most 256 bytes on character boundaries, followed by
    "and N more" when needed. Names share the 1 KiB message envelope, so long names
    may be shortened further to preserve all ten slots and the omission count.

28. **Artist credits and person membership.** TrackInfo.artist remains the display credit. TrackInfo.artists represents ordered, deduplicated name identities, preferring explicit ARTISTS values over ARTIST values. Repeated values and format-defined ID3v2.4 NUL separators are recognized. ID3v2.3 explicit ARTISTS values are split on '/' (the Picard v2.3 join); no other field is split on punctuation. ID3v2.4 ARTISTS values containing a slash remain intact, and words inside a value are never used as separators. Existing album keys, album grouping, artist queries and search defaults retain their meaning. Person-view counts deduplicate tracks and album keys. Missing new fields do not establish a complete person index. Failed reads preserve previous membership and remain eligible for backfill. Artist keys are opaque, equality-only name identities, not verified real-world identities; person queries use name. Per-track names preserve the spelling read from each track within limits.max_artist_name_bytes (8192 UTF-8 bytes, truncated only at character boundaries). QueryResult.Artists.indexing indicates incomplete membership only among available, visible tracks in active roots. Offline or released active roots remain pending; removing and restoring roots does not falsely establish completeness. This signal is independent of rescan_recommended. Backfill preserves display credits, grouping and AlbumKey. Stale scan results cannot overwrite newer membership facts or falsely establish artists_complete after a tag write whose readback failed.

    **Implementation notes (non-normative; may change).** The current implementation
    uses parser-version markers, per-track metadata revisions and a person-index
    revision. Unchanged-file backfill updates membership and its revision only.
    Removed roots retain parser markers; successful tag writes advance metadata
    revisions even when readback fails. Keys currently hash normalized names with
    an identity-version prefix independent of parser versions. The person-view
    summary selects the lexicographically smallest stored spelling for each identity.

29. **Embedded cover editing.** EditCover edits only the explicitly selected files and the documented front-cover representation of their primary picture container. Other pictures, unrelated metadata and audio data are preserved or the file is refused. Removal may reveal an existing fallback image. Preview is read-only and is not a reservation. File-writing permission, canonical target checks, file leases and confirmation rules apply to edits and undo. Every replacement has durable recovery data before it occurs. Recovery reconciles known before/after contents and never overwrites conflicting external edits. Successful library updates and their journal facts are committed together; failures may leave a recoverable intent, never a falsely reported rollback. CoversChanged invalidates cached resources; existing track/library notifications remain available to clients. TrackInfo.cover_revision is an opaque equality-only token. Null means no verified revision, not no cover; has_cover reports presence. CoversChanged and TracksChanged invalidate resources even if that token is null or unchanged.

    **Implementation notes (non-normative; may change).** Cover editing and undo
    currently record an embedded-picture digest. Ordinary scan updates clear it
    because they do not verify final image content or folder-art changes; a null
    token therefore cannot establish resource freshness. Cover reads currently do
    not persist a new revision token.

30. **Playback limits.** Playback limits belong to core and survive client disconnects, not core restarts. Stop-after-current binds to the accepted playback instance and prevents an automatic successor, including repeat-one; success establishes the boundary before acknowledging the command. Manual replacement cancels that boundary, while pause, seek and output recovery preserve it. A sleep timer uses elapsed time and is not shifted by civil-clock changes. It includes system suspend when a suspend-aware clock is available; otherwise core remains usable with a monotonic fallback and reports sleep_timer.includes_suspend=false (limited precision). Clients must refresh PlaybackLimits after wake or returning to visibility rather than trusting display-clock interpolation across suspend. Expiry pauses without restarting audio. The first reached condition consumes both conditions; a coincident natural end stops. Explicit Stop cancels both. Automatic effects have null origin and publish authoritative updated state. Limits never change queue order, persisted gain settings or the meanings of existing transport commands.

31. **Output selection.** The default selection follows the system default as before. Fixed selections use opaque identities, never display-name matching. Requested configuration and active output are reported separately. A successful setting command commits the preference, not proof of active audio; application failures remain queryable. Fixed-device loss pauses and retains the selection. Device arrival does not authorize playback. Explicit default-route changes retain the existing transport policy, with confirmed disconnect evidence taking precedence. Session-only identities are not restored across core runs. Output reconstruction preserves the queue and heard position without duplicating history, and stale stream/configuration results cannot replace newer state.

32. **Output modes and bit-perfect reporting.** Mode configuration describes a request, while output state and signal paths describe verified active facts. Advertising output_mode_contract does not advertise an exclusive backend. Only configurations the build/backend cannot perform at all are refused with output_mode_unsupported before changing preferences. Runtime inability of the selected or later-selected device to satisfy a committed configuration is reported through OutputState.failure/fallback and never rejects SetOutputDevice. Exclusive access alone does not establish bit-perfect output. Yes requires a lossless PCM source, no sample-altering DSP or gain, preserved effective bit depth and channels, a matching sample rate, active share_mode=exclusive, exclusive_verified=true and sample_integrity=verified. Verification covers unchanged delivery of rendered samples to the endpoint driver boundary only, not processing inside drivers, firmware or DACs. Clients use SignalPathInfo.bit_perfect as the sole verdict; capability bit_exact_transport and sample_integrity are evidence components, not conclusions. Known sample alterations retain a No verdict and stable causes; an unverified path remains uncertain, and shared clean output retains shared_mixer. Existing mute commands retain their effects. Existing volume and ReplayGain commands retain their effects in shared mode. In exclusive mode, they retain their effects unless the user explicitly selects a volume mode that disables software gain; any such future mode must be reported in AudioSettings and document its effects on volume commands when introduced. Gain is judged by what is applied, not merely by the selected mode. Shared fallback requires an explicit policy and is reported with the actual format and verdict. Output failures do not mark tracks damaged or skip them. Stopping releases exclusive access. Format changes may interrupt gapless output but must not duplicate heard time or play a successor through the wrong format. API 1.0 does not introduce such an additional volume mode.

    **Implementation notes (non-normative; may change within the guarantees above).**
    The cpal build currently supports only shared/mix: exclusive/source requests are
    refused without changing settings. Its fallback preference is stored but inactive.
