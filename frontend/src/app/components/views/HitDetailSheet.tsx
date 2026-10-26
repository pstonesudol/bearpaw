import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  AudioLines,
  Check,
  Loader2,
  Mic,
  Pencil,
  RefreshCw,
  Trash2,
  X,
} from 'lucide-react';
import { toast } from 'sonner';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { cn } from '../../../lib/utils';
import { useStore } from '../../../store/useStore';
import {
  deleteRecording,
  editTranscript,
  getHit,
  hitAudioUrl,
  transcribeHit,
} from '../../../api/recordings';
import type { ActivityLogEntry, HitDetail, TranscriptStatus } from '../../../types';

export interface HitDetailSheetProps {
  /** Durable hit id, or a numeric row id for history recorded before recordings. */
  hitUid: string | null;
  /** The list row that opened this sheet, so the header renders immediately. */
  initial?: ActivityLogEntry | HitDetail | null;
  onClose: () => void;
}

const TRANSCRIPT_LABEL: Record<TranscriptStatus, string> = {
  none: 'Not transcribed',
  pending: 'Queued',
  processing: 'Transcribing…',
  completed: 'Transcribed',
  failed: 'Transcription failed',
};

const RECORDING_LABEL: Record<string, string> = {
  none: 'No recording',
  recording: 'Recording…',
  ready: 'Recorded',
  partial: 'Partial recording',
  too_short: 'Too short to keep',
  failed: 'Recording failed',
  deleted: 'Recording deleted',
  missing: 'Recording file missing',
};

export function formatFrequency(frequency: number): string {
  return `${frequency.toFixed(4)} MHz`;
}

export function formatDuration(seconds?: number | null): string {
  if (seconds === undefined || seconds === null || !Number.isFinite(seconds)) return '—';
  const total = Math.max(0, seconds);
  if (total < 60) return `${total.toFixed(1)}s`;
  const minutes = Math.floor(total / 60);
  const rest = Math.round(total % 60);
  return `${minutes}m ${rest}s`;
}

function formatTimestamp(seconds?: number | null): string {
  if (!seconds) return '—';
  return new Date(seconds * 1000).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'medium',
  });
}

function hasPlayableAudio(hit: HitDetail | null): boolean {
  return (
    hit !== null &&
    (hit.recording_status === 'ready' || hit.recording_status === 'partial') &&
    !!(hit.hit_uid ?? hit.id)
  );
}

export function HitDetailSheet({ hitUid, initial, onClose }: HitDetailSheetProps) {
  const [hit, setHit] = useState<HitDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const [busy, setBusy] = useState<'transcribe' | 'delete' | 'save' | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const updateActivityLogEntry = useStore((state) => state.updateActivityLogEntry);
  const audioRef = useRef<HTMLAudioElement | null>(null);

  const load = useCallback(async () => {
    if (!hitUid) return;
    setLoading(true);
    try {
      const loaded = await getHit(hitUid);
      setHit(loaded);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not load this hit');
    } finally {
      setLoading(false);
    }
  }, [hitUid]);

  useEffect(() => {
    if (!hitUid) return;
    // Opening a different hit is a dialog lifecycle event, not derived state.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setEditing(false);
    setConfirmDelete(false);
    void load();
  }, [hitUid, load]);

  // Transcription happens on a backend thread; poll only while there is
  // something to wait for. A finished job also arrives over the WebSocket, but
  // this sheet is not guaranteed to be looking at the activity log.
  const status = hit?.transcript_status;
  useEffect(() => {
    if (!hitUid || (status !== 'pending' && status !== 'processing')) return;
    const timer = window.setInterval(() => void load(), 1500);
    return () => window.clearInterval(timer);
  }, [hitUid, status, load]);

  const audioAvailable = hasPlayableAudio(hit);
  const confidenceWarning =
    hit?.link_confidence === 'uncertain' || hit?.recording_status === 'partial';
  const machineText = hit?.transcript_original;
  const transcriptText = hit?.transcript ?? '';

  const title = useMemo(() => {
    const source = hit ?? initial;
    return source?.alpha_tag?.trim() || (source ? formatFrequency(source.frequency) : 'Hit');
  }, [hit, initial]);

  const handleTranscribe = async (force: boolean) => {
    if (!hitUid) return;
    setBusy('transcribe');
    try {
      await transcribeHit(hitUid, force);
      toast.success(force ? 'Transcription queued again' : 'Transcription queued');
      // Reflect it immediately rather than waiting for the first poll.
      setHit((prev) => (prev ? { ...prev, transcript_status: 'pending' } : prev));
      updateActivityLogEntry(hitUid, { transcript_status: 'pending' });
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not queue transcription');
    } finally {
      setBusy(null);
    }
  };

  const handleSave = async () => {
    if (!hitUid) return;
    setBusy('save');
    try {
      const updated = await editTranscript(hitUid, draft);
      setHit(updated);
      setEditing(false);
      updateActivityLogEntry(hitUid, {
        transcript: updated.transcript ?? null,
        transcript_status: 'completed',
      });
      toast.success('Transcript corrected');
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not save the transcript');
    } finally {
      setBusy(null);
    }
  };

  const handleDelete = async () => {
    if (!hitUid) return;
    setBusy('delete');
    try {
      await deleteRecording(hitUid);
      audioRef.current?.pause();
      setConfirmDelete(false);
      updateActivityLogEntry(hitUid, {
        recording_status: 'deleted',
        hasAudio: false,
        transcript: null,
        transcript_status: 'none',
      });
      await load();
      toast.success('Recording and transcript deleted. The hit remains in history.');
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not delete the recording');
    } finally {
      setBusy(null);
    }
  };

  return (
    <Dialog open={hitUid !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-2xl border-white/10 bg-[#12151b] text-white">
        <DialogTitle className="pr-8 text-xl font-bold">
          {title}
          <span className="ml-2 font-mono text-sm font-normal text-white/50">
            {hit
              ? formatFrequency(hit.frequency)
              : initial
                ? formatFrequency(initial.frequency)
                : ''}
          </span>
        </DialogTitle>

        {loading && !hit ? (
          <div className="flex items-center gap-2 py-8 text-sm text-white/60">
            <Loader2 className="size-4 animate-spin" aria-hidden /> Loading hit…
          </div>
        ) : error && !hit ? (
          <div className="space-y-3 py-4">
            <p role="alert" className="text-sm text-red-300">
              {error}
            </p>
            <button
              type="button"
              onClick={() => void load()}
              className="rounded border border-white/10 bg-white/5 px-3 py-1.5 text-sm hover:bg-white/10"
            >
              Try again
            </button>
          </div>
        ) : hit ? (
          <div className="max-h-[70vh] space-y-5 overflow-y-auto pr-1 pb-2">
            <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm sm:grid-cols-3">
              <div>
                <dt className="text-white/50">Channel</dt>
                <dd>{hit.channel ?? '—'}</dd>
              </div>
              <div>
                <dt className="text-white/50">Bank</dt>
                <dd>{hit.bank ?? '—'}</dd>
              </div>
              <div>
                <dt className="text-white/50">Duration</dt>
                <dd>{formatDuration(hit.duration)}</dd>
              </div>
              <div className="col-span-2">
                <dt className="text-white/50">Started</dt>
                <dd>{formatTimestamp(hit.timestamp)}</dd>
              </div>
              <div>
                <dt className="text-white/50">Ended</dt>
                <dd>{formatTimestamp(hit.ended_at)}</dd>
              </div>
              <div>
                <dt className="text-white/50">Scanner</dt>
                <dd>{hit.scanner_id ?? '—'}</dd>
              </div>
            </dl>

            <section aria-label="Recording" className="space-y-2">
              <div className="flex items-center justify-between">
                <h3 className="flex items-center gap-2 text-sm font-bold uppercase tracking-wider text-white/70">
                  <Mic aria-hidden className="size-4" /> Recording
                </h3>
                <span
                  className={cn(
                    'text-xs',
                    hit.recording_status === 'ready'
                      ? 'text-emerald-300'
                      : hit.recording_status === 'partial'
                        ? 'text-amber-300'
                        : 'text-white/50',
                  )}
                >
                  {RECORDING_LABEL[hit.recording_status ?? 'none']}
                </span>
              </div>

              {audioAvailable ? (
                <audio
                  ref={audioRef}
                  controls
                  preload="metadata"
                  src={hitAudioUrl(hit.hit_uid ?? hit.id)}
                  className="w-full"
                >
                  Your browser cannot play this recording.
                </audio>
              ) : (
                <p className="rounded border border-white/10 bg-black/20 px-3 py-2 text-sm text-white/60">
                  {hit.recording_error
                    ? hit.recording_error
                    : hit.recording_status === 'recording'
                      ? 'The backend is still writing this transmission.'
                      : hit.recording_status === 'too_short'
                        ? 'Shorter than the configured minimum recording length.'
                        : 'No audio is stored for this hit.'}
                </p>
              )}

              {audioAvailable && (
                <p className="text-xs text-white/45">
                  {formatDuration(hit.recording_duration)} of audio
                  {hit.recording_bytes
                    ? ` · ${Math.max(1, Math.round(hit.recording_bytes / 1024))} KB`
                    : ''}
                </p>
              )}

              {confidenceWarning && (
                <p className="flex items-start gap-2 rounded border border-amber-400/20 bg-amber-500/10 p-2 text-xs leading-relaxed text-amber-100">
                  <AlertTriangle aria-hidden className="mt-0.5 size-3.5 shrink-0" />
                  The scanner changed or the audio device dropped out during this transmission. The
                  file is real, but it may not be the whole transmission.
                </p>
              )}
            </section>

            <section aria-label="Transcript" className="space-y-2">
              <div className="flex items-center justify-between gap-3">
                <h3 className="flex items-center gap-2 text-sm font-bold uppercase tracking-wider text-white/70">
                  <AudioLines aria-hidden className="size-4" /> Transcript
                </h3>
                <span
                  className={cn(
                    'text-xs',
                    hit.transcript_status === 'completed'
                      ? 'text-emerald-300'
                      : hit.transcript_status === 'failed'
                        ? 'text-red-300'
                        : 'text-white/50',
                  )}
                >
                  {TRANSCRIPT_LABEL[(hit.transcript_status ?? 'none') as TranscriptStatus]}
                </span>
              </div>

              {editing ? (
                <div className="space-y-2">
                  <textarea
                    aria-label="Transcript text"
                    value={draft}
                    onChange={(event) => setDraft(event.target.value)}
                    rows={5}
                    className="w-full rounded border border-white/15 bg-black/30 p-2 text-sm text-white focus:border-brand-primary focus:outline-none"
                  />
                  <div className="flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void handleSave()}
                      disabled={busy === 'save'}
                      className="inline-flex items-center gap-1.5 rounded bg-brand-primary px-3 py-1.5 text-sm font-bold text-white disabled:opacity-50"
                    >
                      {busy === 'save' ? (
                        <Loader2 aria-hidden className="size-4 animate-spin" />
                      ) : (
                        <Check aria-hidden className="size-4" />
                      )}
                      Save correction
                    </button>
                    <button
                      type="button"
                      onClick={() => setEditing(false)}
                      className="rounded border border-white/10 px-3 py-1.5 text-sm text-white/70 hover:bg-white/5"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              ) : transcriptText ? (
                <p
                  lang="en"
                  className="whitespace-pre-wrap rounded border border-white/10 bg-black/20 p-3 text-sm leading-relaxed text-white/85"
                >
                  {transcriptText}
                </p>
              ) : (
                <p className="rounded border border-white/10 bg-black/20 px-3 py-2 text-sm text-white/60">
                  {hit.transcript_error ? hit.transcript_error : 'No transcript yet.'}
                </p>
              )}

              {machineText && machineText !== transcriptText && !editing && (
                <details className="text-xs text-white/50">
                  <summary className="cursor-pointer select-none">Machine transcript</summary>
                  <p className="mt-1 whitespace-pre-wrap border-l border-white/10 pl-3">
                    {machineText}
                  </p>
                </details>
              )}

              <div className="flex flex-wrap items-center gap-2 pt-1">
                {!editing && (
                  <button
                    type="button"
                    onClick={() => {
                      setDraft(transcriptText);
                      setEditing(true);
                    }}
                    className="inline-flex items-center gap-1.5 rounded border border-white/10 bg-white/5 px-3 py-1.5 text-sm hover:bg-white/10"
                  >
                    <Pencil aria-hidden className="size-4" /> Correct transcript
                  </button>
                )}
                <button
                  type="button"
                  onClick={() => void handleTranscribe(!!transcriptText)}
                  disabled={!audioAvailable || busy === 'transcribe'}
                  className="inline-flex items-center gap-1.5 rounded border border-white/10 bg-white/5 px-3 py-1.5 text-sm hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-50"
                >
                  {busy === 'transcribe' ? (
                    <Loader2 aria-hidden className="size-4 animate-spin" />
                  ) : (
                    <RefreshCw aria-hidden className="size-4" />
                  )}
                  {transcriptText ? 'Transcribe again' : 'Retry transcription'}
                </button>
                {hit.hit_uid && (
                  <span className="ml-auto font-mono text-[11px] text-white/30">{hit.hit_uid}</span>
                )}
              </div>
            </section>

            <section
              aria-label="Delete recording"
              className="flex items-center justify-between gap-4 border-t border-white/10 pt-4"
            >
              <p className="text-xs text-white/50">
                Deleting removes the audio and transcript. The hit, frequency and timestamps stay in
                history.
              </p>
              {confirmDelete ? (
                <div className="flex shrink-0 items-center gap-2">
                  <button
                    type="button"
                    onClick={() => void handleDelete()}
                    disabled={busy === 'delete'}
                    className="inline-flex items-center gap-1.5 rounded bg-red-600 px-3 py-1.5 text-sm font-bold text-white hover:bg-red-500 disabled:opacity-50"
                  >
                    {busy === 'delete' ? (
                      <Loader2 aria-hidden className="size-4 animate-spin" />
                    ) : (
                      <Trash2 aria-hidden className="size-4" />
                    )}
                    Delete
                  </button>
                  <button
                    type="button"
                    onClick={() => setConfirmDelete(false)}
                    className="rounded border border-white/10 px-3 py-1.5 text-sm text-white/70 hover:bg-white/5"
                  >
                    Keep
                  </button>
                </div>
              ) : (
                <button
                  type="button"
                  onClick={() => setConfirmDelete(true)}
                  disabled={!hit.hit_uid || !audioAvailable}
                  className="inline-flex shrink-0 items-center gap-1.5 rounded border border-red-500/30 px-3 py-1.5 text-sm text-red-200 hover:bg-red-500/10 disabled:cursor-not-allowed disabled:opacity-40"
                >
                  <Trash2 aria-hidden className="size-4" /> Delete recording
                </button>
              )}
            </section>
          </div>
        ) : (
          <div className="flex items-center gap-2 py-6 text-sm text-white/60">
            <X aria-hidden className="size-4" /> This hit is no longer available.
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
