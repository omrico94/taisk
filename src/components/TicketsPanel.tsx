import { useCallback, useEffect, useMemo, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { useShallow } from "zustand/react/shallow";
import {
  connectTracker,
  disconnectTracker,
  getTickets,
  getTrackerLinks,
  getTrackers,
  getTrackerSuggestions,
  setTrackerLinks,
} from "../api";
import { openExternal } from "../openExternal";
import { useSessionStore } from "../store/sessionStore";
import type { Ticket, TicketsResponse, TrackerInfo, TrackerLink } from "../types";
import { beginDrag, dropTicket, endDrag } from "./boardActions";
import styles from "./TicketsPanel.module.css";

const errText = (err: unknown) => (err instanceof Error ? err.message : String(err));

// Ticket trackers → tasks. Provider-neutral on purpose: every provider-specific
// string (name, auth fields, what a "container" is called) comes from
// `GET /trackers`, so a new tracker needs no change here. Import is always an
// explicit action — "Add" (→ the board's intake column) or dragging a row onto a column.
export function TicketsPanel() {
  const open = useSessionStore((s) => s.ticketsOpen);
  const setOpen = useSessionStore((s) => s.setTicketsOpen);
  const board = useSessionStore((s) => s.activeBoardId);
  const trackers = useSessionStore(useShallow((s) => s.trackers));
  const setTrackers = useSessionStore((s) => s.setTrackers);
  const draggingTicket = useSessionStore((s) => s.drag.kind === "ticket");
  // "Add" files into the board's intake column (the same one quick-add uses).
  const intakeName = useSessionStore((s) => {
    const layout = s.columns[s.activeBoardId] ?? s.defaultColumns;
    const id = layout.intake ?? layout.columns[0]?.id;
    return layout.columns.find((c) => c.id === id)?.name ?? "the first column";
  });
  // Re-list when tasks change so "on board" markers follow imports/deletes.
  const taskCount = useSessionStore((s) => s.tasks.length);

  const [links, setLinks] = useState<TrackerLink[]>([]);
  const [tickets, setTickets] = useState<TicketsResponse>({ tickets: [], errors: [] });
  const [loading, setLoading] = useState(false);
  const [filter, setFilter] = useState("");
  const [error, setError] = useState<string | null>(null);

  const refreshTrackers = useCallback(() => {
    getTrackers()
      .then(setTrackers)
      .catch((err) => setError(errText(err)));
  }, [setTrackers]);

  const refreshTickets = useCallback(() => {
    setLoading(true);
    getTickets(board)
      .then(setTickets)
      .catch((err) => setError(errText(err)))
      .finally(() => setLoading(false));
  }, [board]);

  useEffect(() => {
    if (!open) return;
    setError(null);
    refreshTrackers();
    getTrackerLinks(board)
      .then(setLinks)
      .catch((err) => setError(errText(err)));
  }, [open, board, refreshTrackers]);

  useEffect(() => {
    if (open && links.length > 0) refreshTickets();
    else setTickets({ tickets: [], errors: [] });
  }, [open, links, taskCount, refreshTickets]);

  const saveLinks = async (next: TrackerLink[]) => {
    try {
      setLinks(await setTrackerLinks(board, next));
      setError(null);
    } catch (err) {
      setError(errText(err));
    }
  };

  const visible = useMemo(() => {
    const f = filter.trim().toLowerCase();
    if (!f) return tickets.tickets;
    return tickets.tickets.filter((t) =>
      [t.key, t.title, t.container, ...t.labels].some((s) => s.toLowerCase().includes(f)),
    );
  }, [tickets, filter]);

  const connected = trackers.filter((t) => t.status.connected);

  return (
    <AnimatePresence>
      {open && (
        <motion.aside
          className={`${styles.drawer} ${draggingTicket ? styles.dragging : ""}`}
          role="dialog"
          aria-label="Tickets"
          data-testid="tickets-panel"
          initial={{ x: 40, opacity: 0 }}
          animate={{ x: 0, opacity: 1 }}
          exit={{ x: 40, opacity: 0 }}
          transition={{ duration: 0.22 }}
        >
          <div className={styles.header}>
            <div className={styles.title}>Tickets</div>
            <button className={styles.close} onClick={() => setOpen(false)} aria-label="Close">
              ✕
            </button>
          </div>

          <section className={styles.section}>
            {trackers.map((t) => (
              <TrackerConnection key={t.id} tracker={t} onChanged={refreshTrackers} />
            ))}
          </section>

          {connected.map((t) => (
            <LinkEditor
              key={t.id}
              tracker={t}
              board={board}
              links={links}
              onSave={(next) => void saveLinks(next)}
            />
          ))}

          {links.length > 0 && (
            <section className={`${styles.section} ${styles.listSection}`}>
              <div className={styles.listHead}>
                <input
                  className={styles.input}
                  placeholder="Filter tickets…"
                  value={filter}
                  onChange={(e) => setFilter(e.target.value)}
                  aria-label="Filter tickets"
                />
                <button className={styles.small} onClick={refreshTickets} disabled={loading}>
                  {loading ? "Loading…" : "Refresh"}
                </button>
              </div>
              {tickets.errors.map((e) => (
                <div className={styles.error} key={`${e.provider}:${e.container}`}>
                  {e.container}: {e.error}
                </div>
              ))}
              <div className={styles.hint}>Drag a ticket onto a column, or add it to {intakeName}.</div>
              <div className={styles.list}>
                {visible.map((t) => (
                  <TicketRow key={`${t.provider}:${t.key}`} ticket={t} showContainer={links.length > 1} />
                ))}
                {!loading && visible.length === 0 && (
                  <div className={styles.empty}>{filter ? "No matching open tickets." : "No open tickets."}</div>
                )}
              </div>
            </section>
          )}

          {error && <div className={styles.error}>{error}</div>}
        </motion.aside>
      )}
    </AnimatePresence>
  );
}

function TrackerConnection({ tracker, onChanged }: { tracker: TrackerInfo; onChanged: () => void }) {
  const { status } = tracker;
  const [editing, setEditing] = useState(false);
  const [fields, setFields] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const connect = async () => {
    setBusy(true);
    setError(null);
    try {
      await connectTracker(tracker.id, fields);
      setFields({});
      setEditing(false);
      onChanged();
    } catch (err) {
      setError(errText(err));
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async () => {
    try {
      await disconnectTracker(tracker.id);
      onChanged();
    } catch (err) {
      setError(errText(err));
    }
  };

  const showForm = editing || (!status.connected && !status.source);
  const via = status.source === "keychain" ? "saved token" : status.source ? `${status.source} CLI` : null;

  return (
    <div className={styles.tracker} data-testid={`tracker-${tracker.id}`}>
      <div className={styles.trackerHead}>
        <span className={`${styles.dot} ${status.connected ? styles.dotOn : ""}`} />
        <span className={styles.trackerName}>{tracker.name}</span>
        <span className={styles.trackerStatus}>
          {status.connected ? `@${status.account}${via ? ` · via ${via}` : ""}` : status.error ? "not working" : "not connected"}
        </span>
        <span className={styles.spacer} />
        {status.source === "keychain" && (
          <button className={styles.small} onClick={() => void disconnect()}>
            Disconnect
          </button>
        )}
        {!showForm && status.source !== "keychain" && (
          <button className={styles.small} onClick={() => setEditing(true)}>
            Use a token
          </button>
        )}
      </div>
      {status.error && <div className={styles.error}>{status.error}</div>}
      {showForm && (
        <div className={styles.form}>
          {tracker.auth_fields.map((f) => (
            <label key={f.name} className={styles.field}>
              <span className={styles.fieldLabel}>{f.label}</span>
              <input
                className={styles.input}
                type={f.secret ? "password" : "text"}
                autoComplete="off"
                value={fields[f.name] ?? ""}
                onChange={(e) => setFields({ ...fields, [f.name]: e.target.value })}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void connect();
                }}
              />
              {(f.help || f.help_url) && (
                <span className={styles.note}>
                  {f.help}{" "}
                  {f.help_url && (
                    <button className={styles.link} onClick={() => openExternal(f.help_url!)}>
                      Create one
                    </button>
                  )}
                </span>
              )}
            </label>
          ))}
          <div className={styles.formFoot}>
            <span className={styles.note}>Stored in your system keychain only.</span>
            {editing && (
              <button className={styles.small} onClick={() => setEditing(false)}>
                Cancel
              </button>
            )}
            <button className={styles.primary} disabled={busy} onClick={() => void connect()}>
              {busy ? "Checking…" : "Connect"}
            </button>
          </div>
          {error && <div className={styles.error}>{error}</div>}
        </div>
      )}
    </div>
  );
}

function LinkEditor({
  tracker,
  board,
  links,
  onSave,
}: {
  tracker: TrackerInfo;
  board: string;
  links: TrackerLink[];
  onSave: (next: TrackerLink[]) => void;
}) {
  const [text, setText] = useState("");
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const mine = links.filter((l) => l.provider === tracker.id);

  useEffect(() => {
    getTrackerSuggestions(tracker.id, board)
      .then(setSuggestions)
      .catch(() => setSuggestions([]));
  }, [tracker.id, board]);

  const add = (container: string) => {
    if (!container.trim()) return;
    onSave([...links, { provider: tracker.id, container }]);
    setText("");
  };
  const remove = (container: string) =>
    onSave(links.filter((l) => !(l.provider === tracker.id && l.container === container)));
  const unlinked = suggestions.filter((s) => !mine.some((l) => l.container === s));

  return (
    <section className={styles.section}>
      <div className={styles.sectionTitle}>
        {tracker.name} · linked to this board
      </div>
      <div className={styles.chips}>
        {mine.map((l) => (
          <span className={styles.chip} key={l.container}>
            {l.container}
            <button className={styles.chipX} onClick={() => remove(l.container)} aria-label={`Unlink ${l.container}`}>
              ✕
            </button>
          </span>
        ))}
        {unlinked.map((s) => (
          <button className={styles.suggestion} key={s} onClick={() => add(s)} title="From this board's sessions">
            + {s}
          </button>
        ))}
      </div>
      <div className={styles.listHead}>
        <input
          className={styles.input}
          placeholder={tracker.container_placeholder}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") add(text);
          }}
          aria-label={`Link a ${tracker.container_label.toLowerCase()}`}
        />
        <button className={styles.small} disabled={!text.trim()} onClick={() => add(text)}>
          Link
        </button>
      </div>
    </section>
  );
}

function TicketRow({ ticket, showContainer }: { ticket: Ticket; showContainer: boolean }) {
  const imported = ticket.imported_task_id !== null;
  const ref = { provider: ticket.provider, key: ticket.key };
  return (
    <div
      className={`${styles.ticket} ${imported ? styles.ticketImported : ""}`}
      data-testid="ticket-row"
      draggable={!imported}
      onDragStart={(e) => {
        e.dataTransfer.setData("text/plain", ticket.key);
        e.dataTransfer.effectAllowed = "copy";
        beginDrag({ kind: "ticket", taskId: null, sessId: null, srcTaskId: null, ticket: ref });
      }}
      onDragEnd={endDrag}
    >
      <div className={styles.ticketMain}>
        <div className={styles.ticketTitle}>{ticket.title}</div>
        <div className={styles.ticketMeta}>
          <span className={styles.ticketKey}>{showContainer ? ticket.key : ticket.key.replace(/^.*(?=#)/, "")}</span>
          {ticket.labels.slice(0, 3).map((l) => (
            <span className={styles.label} key={l}>
              {l}
            </span>
          ))}
          {ticket.assignee && <span className={styles.assignee}>@{ticket.assignee}</span>}
        </div>
      </div>
      {imported ? (
        <span className={styles.onBoard}>on board</span>
      ) : (
        <button className={styles.small} onClick={() => dropTicket(ref)} data-testid="import-ticket">
          Add
        </button>
      )}
    </div>
  );
}
