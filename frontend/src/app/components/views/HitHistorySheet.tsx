import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  ChevronLeft,
  ChevronRight,
  Loader2,
  Mic,
  Pause,
  Play,
  RefreshCw,
  Search,
} from 'lucide-react';
import { toast } from 'sonner';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { cn } from '../../../lib/utils';
import {
  getHits,
  getRecordingsSummary,
  hitAudioUrl,
  transcribeHit,
  type HitQuery,
} from '../../../api/recordings';
import type { HitDetail, RecordingsSummary, TranscriptStatus } from '../../../types';
import { formatDuration } from './HitDetailSheet';

export interface HitHistorySheetProps {
  isOpen: boolean;
  onClose: () => void;
  onSelectHit: (id: string) => void;
}

const PAGE_SIZE = 25;

const TRANSCRIPT_OPTIONS: { value: string; label: string }[] = [
  { value: 'any', label: 'Any transcript' },
  { value: 'completed', label: 'Transcribed' },
  { value: 'pending', label: 'Queued' },
  { value: 'processing', label: 'In progress' },
  { value: 'failed', label: 'Failed' },
  { value: 'none', label: 'Not transcribed' },
];

const RECORDING_OPTIONS = [
  { value: 'any', label: 'Any recording' },
  { value: 'yes', label: 'Has audio' },
  { value: 'no', label: 'No audio' },
] as const;

type RecordingFilter = (typeof RECORDING_OPTIONS)[number]['value'];

const statusDot: Record<TranscriptStatus, string> = {
  none: 'bg-white/20',
  pending: 'bg-amber-400',
  processing: 'bg-amber-400 animate-pulse',
  completed: 'bg-emerald-400',
  failed: 'bg-red-400',
};

const STATUS_LABEL: Record<TranscriptStatus, string> = {
  completed: 'Transcribed',
  failed: 'Transcription failed',
  none: 'Not transcribed',
  pending: 'Transcription queued',
  processing: 'Transcription in progress',
};

function dayStart(value: string): number | undefined {
  if (!value) return undefined;
  const date = new Date(`${value}T00:00:00`);
  return Number.isNaN(date.getTime()) ? undefined : Math.floor(date.getTime() / 1000);
}

function dayEnd(value: string): number | undefined {
  if (!value) return undefined;
  const date = new Date(`${value}T23:59:59`);
  return Number.isNaN(date.getTime()) ? undefined : Math.floor(date.getTime() / 1000);
}

function hasAudio(hit: HitDetail): boolean {
  return hit.recording_status === 'ready' || hit.recording_status === 'partial';
}

function hitId(hit: HitDetail): string {
  return hit.hit_uid ?? hit.id;
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 KB';
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

/** Coarse "how old is this" for the header, where the exact second is noise. */
function formatAge(timestamp: number): string {
  const seconds = Math.max(0, Date.now() / 1000 - timestamp);
  if (seconds < 90) return 'just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

interface DayGroup {
  key: string;
  label: string;
  hits: HitDetail[];
  audioSeconds: number;
}

/**
 * Group a page of hits by local calendar day.
 *
 * The list is ordered newest-first, so a page's days are contiguous and grouping
 * is a single pass. Totals describe the rows in view, not the whole day: the
 * page footer still reports what slice of the total this is.
 */
function groupByDay(hits: HitDetail[]): DayGroup[] {
  const groups: DayGroup[] = [];
  for (const hit of hits) {
    const date = new Date(hit.timestamp * 1000);
    const key = `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
    let group = groups[groups.length - 1];
    if (!group || group.key !== key) {
      group = {
        key,
        label: date.toLocaleDateString(undefined, {
          weekday: 'short',
          month: 'short',
          day: 'numeric',
        }),
        hits: [],
        audioSeconds: 0,
      };
      groups.push(group);
    }
    group.hits.push(hit);
    if (hasAudio(hit)) group.audioSeconds += hit.recording_duration ?? hit.duration ?? 0;
  }
  return groups;
}

export function HitHistorySheet({ isOpen, onClose, onSelectHit }: HitHistorySheetProps) {
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [recording, setRecording] = useState<RecordingFilter>('any');
  const [transcriptStatus, setTranscriptStatus] = useState('any');
  const [channel, setChannel] = useState('');
  const [offset, setOffset] = useState(0);
  const [hits, setHits] = useState<HitDetail[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [summary, setSummary] = useState<RecordingsSummary | null>(null);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [playing, setPlaying] = useState(false);
  const [playError, setPlayError] = useState<string | null>(null);
  const [retrying, setRetrying] = useState(false);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const requestId = useRef(0);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setDebouncedSearch(search.trim());
      setOffset(0);
    }, 300);
    return () => window.clearTimeout(timer);
  }, [search]);

  // Every filter change starts a new page: keeping offset 2 while narrowing to
  // three results shows an empty list that looks like a broken search. Applied
  // in each handler rather than an effect so the reset lands in the same render
  // as the filter.
  const resetPage = useCallback(() => setOffset(0), []);

  const query = useMemo<HitQuery>(() => {
    const channelNumber = Number.parseInt(channel, 10);
    return {
      search: debouncedSearch || undefined,
      channel: Number.isFinite(channelNumber) ? channelNumber : undefined,
      start_time: dayStart(from),
      end_time: dayEnd(to),
      has_recording: recording === 'any' ? undefined : recording === 'yes',
      transcript_status: transcriptStatus === 'any' ? undefined : transcriptStatus,
      limit: PAGE_SIZE,
      offset,
    };
  }, [debouncedSearch, channel, from, to, recording, transcriptStatus, offset]);

  const load = useCallback(async () => {
    const id = ++requestId.current;
    setLoading(true);
    try {
      const page = await getHits(query);
      if (id !== requestId.current) return;
      setHits(page.hits);
      setTotal(page.total);
      setError(null);
    } catch (e) {
      if (id !== requestId.current) return;
      setError(e instanceof Error ? e.message : 'Could not load history');
    } finally {
      if (id === requestId.current) setLoading(false);
    }
  }, [query]);

  // The header is decoration, so a failed summary fetch must not put the list in
  // an error state; the rows are still usable without it.
  const loadSummary = useCallback(async () => {
    try {
      setSummary(await getRecordingsSummary());
    } catch {
      setSummary(null);
    }
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    // Data-fetch lifecycle, not derived state: `load` sets `loading` before it
    // awaits. Firing it when the dialog opens is the only trigger available.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void load();
    void loadSummary();
  }, [isOpen, load, loadSummary]);

  // Closing the sheet stops playback: the audio is a child of this dialog and
  // must not outlive it. Guarded writes keep the effect idempotent under
  // StrictMode's double-invoke.
  useEffect(() => {
    if (isOpen) return;
    audioRef.current?.pause();
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setPlaying((value) => (value ? false : value));
    setCurrentId((value) => (value === null ? value : null));
  }, [isOpen]);

  const startPlayback = useCallback(
    (id: string) => {
      const audio = audioRef.current;
      if (!audio) return;
      // Only reload the source when the row changes, so play/pause on the same
      // row resumes where it stopped.
      if (currentId !== id) audio.src = hitAudioUrl(id);
      setCurrentId(id);
      setPlayError(null);
      setPlaying(true);
      const started = audio.play();
      // jsdom and older Safari return undefined instead of a promise.
      if (started && typeof started.catch === 'function') {
        started.catch(() => {
          setPlayError('Could not play this recording.');
          setPlaying(false);
        });
      }
    },
    [currentId],
  );

  const togglePlay = useCallback(
    (hit: HitDetail) => {
      const id = hitId(hit);
      const audio = audioRef.current;
      if (!audio) return;
      if (currentId === id && playing) {
        audio.pause();
        setPlaying(false);
        return;
      }
      startPlayback(id);
    },
    [currentId, playing, startPlayback],
  );

  // Auto-advance through the rows in view. A review pass is "play the list", not
  // "click each row", so finishing one recording starts the next playable one.
  const handleEnded = () => {
    setPlaying(false);
    const order = hits.filter(hasAudio).map(hitId);
    const index = currentId ? order.indexOf(currentId) : -1;
    const next = index >= 0 ? order[index + 1] : undefined;
    if (next) {
      startPlayback(next);
    } else {
      setCurrentId(null);
    }
  };

  const retryFailed = async () => {
    setRetrying(true);
    try {
      const page = await getHits({ transcript_status: 'failed', has_recording: true, limit: 50 });
      const ids = page.hits.filter(hasAudio).map(hitId);
      if (ids.length === 0) {
        toast.info('No failed transcripts to retry');
        return;
      }
      await Promise.all(ids.map((id) => transcribeHit(id, true)));
      toast.success(`${ids.length} transcription${ids.length === 1 ? '' : 's'} queued`);
      await Promise.all([load(), loadSummary()]);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not retry transcriptions');
    } finally {
      setRetrying(false);
    }
  };

  const groups = useMemo(() => groupByDay(hits), [hits]);

  // Quick filters are presets over the two select filters, not a third source of
  // truth: choosing one sets those values, and a chip reads as pressed only when
  // the selects match it exactly.
  const quickFilters = useMemo(
    () => [
      { id: 'all', label: 'All' },
      { id: 'audio', label: 'With audio', count: summary?.count },
      { id: 'untranscribed', label: 'No transcript' },
      { id: 'failed', label: 'Failed', count: summary?.failed_transcripts },
    ],
    [summary],
  );
  const activeQuick = useMemo(() => {
    if (recording === 'any' && transcriptStatus === 'any') return 'all';
    if (recording === 'yes' && transcriptStatus === 'none') return 'untranscribed';
    if (recording === 'any' && transcriptStatus === 'failed') return 'failed';
    if (recording === 'yes' && transcriptStatus === 'any') return 'audio';
    return null;
  }, [recording, transcriptStatus]);

  const applyQuick = (id: string) => {
    resetPage();
    if (id === 'all') {
      setRecording('any');
      setTranscriptStatus('any');
    } else if (id === 'audio') {
      setRecording('yes');
      setTranscriptStatus('any');
    } else if (id === 'untranscribed') {
      setRecording('yes');
      setTranscriptStatus('none');
    } else if (id === 'failed') {
      setRecording('any');
      setTranscriptStatus('failed');
    }
  };

  const pageEnd = Math.min(offset + hits.length, total);
  const hasPrevious = offset > 0;
  const hasNext = offset + PAGE_SIZE < total;

  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex max-h-[92vh] max-w-4xl flex-col border-white/10 bg-[#12151b] text-white">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <DialogTitle className="pr-8 text-xl font-bold">Hit History</DialogTitle>
            {summary && (
              <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-white/45">
                <span className="text-white/70">
                  {summary.count} {summary.count === 1 ? 'recording' : 'recordings'}
                </span>
                <span aria-hidden>·</span>
                <span>{formatBytes(summary.bytes)}</span>
                <span aria-hidden>·</span>
                <span>
                  keeping{' '}
                  {summary.retention_days === 0
                    ? 'until hits expire'
                    : `${summary.retention_days} ${summary.retention_days === 1 ? 'day' : 'days'}`}
                </span>
                {summary.oldest_timestamp != null && (
                  <>
                    <span aria-hidden>·</span>
                    <span>oldest {formatAge(summary.oldest_timestamp)}</span>
                  </>
                )}
              </p>
            )}
          </div>
          {summary && summary.failed_transcripts > 0 && (
            <button
              type="button"
              onClick={() => void retryFailed()}
              disabled={retrying}
              className="inline-flex shrink-0 items-center gap-1.5 rounded border border-white/10 px-2.5 py-1 text-xs text-white/80 hover:bg-white/5 disabled:cursor-not-allowed disabled:opacity-50"
            >
              <RefreshCw aria-hidden className={cn('size-3.5', retrying && 'animate-spin')} />
              Retry {summary.failed_transcripts} failed
            </button>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-1.5">
          {quickFilters.map((filter) => (
            <button
              key={filter.id}
              type="button"
              onClick={() => applyQuick(filter.id)}
              aria-pressed={activeQuick === filter.id}
              className={cn(
                'rounded-full border px-2.5 py-1 text-xs transition-colors',
                activeQuick === filter.id
                  ? 'border-brand-primary/50 bg-brand-primary/15 text-brand-light'
                  : 'border-white/10 bg-white/[0.03] text-white/55 hover:bg-white/5',
              )}
            >
              {filter.label}
              {filter.count != null && filter.count > 0 && (
                <span className="ml-1 text-white/40">{filter.count}</span>
              )}
            </button>
          ))}
        </div>

        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <label className="col-span-2 flex items-center gap-2 rounded border border-white/15 bg-black/30 px-2 sm:col-span-2">
            <Search aria-hidden className="size-4 shrink-0 text-white/40" />
            <span className="sr-only">Search channel name, frequency or transcript</span>
            <input
              type="search"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search tag, frequency, transcript…"
              className="w-full bg-transparent py-2 text-sm text-white placeholder:text-white/35 focus:outline-none"
            />
          </label>
          <label className="text-xs text-white/50">
            From
            <input
              type="date"
              value={from}
              onChange={(event) => {
                setFrom(event.target.value);
                resetPage();
              }}
              className="mt-1 w-full rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
            />
          </label>
          <label className="text-xs text-white/50">
            To
            <input
              type="date"
              value={to}
              onChange={(event) => {
                setTo(event.target.value);
                resetPage();
              }}
              className="mt-1 w-full rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
            />
          </label>
          <label className="text-xs text-white/50">
            Channel
            <input
              inputMode="numeric"
              value={channel}
              onChange={(event) => {
                setChannel(event.target.value.replace(/[^0-9]/g, ''));
                resetPage();
              }}
              placeholder="Any"
              className="mt-1 w-full rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
            />
          </label>
          <label className="text-xs text-white/50">
            Recording
            <select
              value={recording}
              onChange={(event) => {
                setRecording(event.target.value as RecordingFilter);
                resetPage();
              }}
              className="mt-1 w-full rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
            >
              {RECORDING_OPTIONS.map((option) => (
                <option key={option.value} value={option.value} className="bg-[#12151b]">
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <label className="text-xs text-white/50">
            Transcript
            <select
              value={transcriptStatus}
              onChange={(event) => {
                setTranscriptStatus(event.target.value);
                resetPage();
              }}
              className="mt-1 w-full rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
            >
              {TRANSCRIPT_OPTIONS.map((option) => (
                <option key={option.value} value={option.value} className="bg-[#12151b]">
                  {option.label}
                </option>
              ))}
            </select>
          </label>
        </div>

        {playError && (
          <p role="alert" className="text-xs text-red-300">
            {playError}
          </p>
        )}

        <audio
          ref={audioRef}
          data-testid="hit-history-audio"
          preload="none"
          onEnded={handleEnded}
          onError={() => {
            setPlayError('Could not play this recording.');
            setPlaying(false);
          }}
          className="hidden"
        />

        <div
          className="max-h-[50vh] min-h-[180px] flex-1 overflow-y-auto rounded border border-white/10"
          aria-busy={loading}
        >
          {loading && hits.length === 0 ? (
            <div className="flex items-center justify-center gap-2 py-16 text-sm text-white/60">
              <Loader2 aria-hidden className="size-4 animate-spin" /> Loading hits…
            </div>
          ) : error ? (
            <div className="space-y-2 py-10 text-center text-sm">
              <p role="alert" className="text-red-300">
                {error}
              </p>
              <button
                type="button"
                onClick={() => void load()}
                className="rounded border border-white/10 px-3 py-1.5 text-white/70 hover:bg-white/5"
              >
                Try again
              </button>
            </div>
          ) : hits.length === 0 ? (
            <div className="py-16 text-center text-sm text-white/50">
              No hits match these filters.
            </div>
          ) : (
            <div className="divide-y divide-white/5">
              {groups.map((group) => (
                <section key={group.key} aria-label={group.label}>
                  <h3 className="sticky top-0 z-10 flex items-baseline justify-between gap-2 border-b border-white/5 bg-[#0f1218]/95 px-3 py-1.5 backdrop-blur">
                    <span className="text-[0.65rem] font-semibold tracking-[0.14em] text-white/70 uppercase">
                      {group.label}
                    </span>
                    <span className="text-[0.7rem] text-white/40">
                      {group.hits.length} {group.hits.length === 1 ? 'hit' : 'hits'}
                      {group.audioSeconds > 0 && ` · ${formatDuration(group.audioSeconds)}`}
                    </span>
                  </h3>
                  <ul className="divide-y divide-white/5">
                    {group.hits.map((hit) => {
                      const id = hitId(hit);
                      const status = (hit.transcript_status ?? 'none') as TranscriptStatus;
                      const playable = hasAudio(hit);
                      const isPlaying = playing && currentId === id;
                      return (
                        <li
                          key={id}
                          className={cn(
                            'flex items-stretch gap-1 pl-2',
                            isPlaying && 'bg-brand-primary/10',
                          )}
                        >
                          {playable ? (
                            <button
                              type="button"
                              onClick={() => togglePlay(hit)}
                              aria-pressed={isPlaying}
                              aria-label={`${isPlaying ? 'Pause' : 'Play'} recording at ${hit.frequency.toFixed(4)} MHz`}
                              className={cn(
                                'my-auto grid size-7 shrink-0 place-items-center rounded-full border transition-colors',
                                isPlaying
                                  ? 'border-brand-primary bg-brand-primary text-[#12151b]'
                                  : 'border-white/15 text-brand-light hover:border-brand-primary/60 hover:bg-white/5',
                              )}
                            >
                              {isPlaying ? (
                                <Pause aria-hidden className="size-3.5" />
                              ) : (
                                <Play aria-hidden className="size-3.5" />
                              )}
                            </button>
                          ) : (
                            <span className="my-auto size-7 shrink-0" aria-hidden />
                          )}
                          <button
                            type="button"
                            onClick={() => onSelectHit(id)}
                            className="grid flex-1 grid-cols-[4.2rem_5.4rem_1fr_auto] items-center gap-3 py-2 pr-2 pl-1 text-left text-sm hover:bg-white/5 focus:bg-white/5 focus:outline-none"
                            aria-label={`Open hit at ${hit.frequency.toFixed(4)} MHz${
                              hit.alpha_tag ? `, ${hit.alpha_tag}` : ''
                            }`}
                          >
                            <span className="text-xs text-white/50 tabular-nums">
                              {new Date(hit.timestamp * 1000).toLocaleTimeString(undefined, {
                                hour: '2-digit',
                                minute: '2-digit',
                                second: '2-digit',
                                hourCycle: 'h23',
                              })}
                            </span>
                            <span className="font-mono text-brand-light">
                              {hit.frequency.toFixed(4)}
                            </span>
                            <span className="min-w-0">
                              <span className="block truncate text-white/80">
                                {hit.alpha_tag || '—'}
                                <span className="ml-2 text-xs text-white/35">
                                  {hit.channel != null ? `ch ${hit.channel}` : ''}
                                </span>
                              </span>
                              {hit.transcript ? (
                                <span className="block truncate text-xs text-white/45">
                                  {hit.transcript}
                                </span>
                              ) : null}
                            </span>
                            <span className="flex items-center gap-2 text-xs text-white/50">
                              <span title={formatDuration(hit.duration)} className="tabular-nums">
                                {formatDuration(hit.duration)}
                              </span>
                              {playable ? (
                                <Mic
                                  aria-label="Has recording"
                                  className="size-3.5 text-brand-primary"
                                />
                              ) : (
                                <span className="size-3.5" aria-hidden />
                              )}
                              <span
                                className={cn('size-2 rounded-full', statusDot[status])}
                                aria-label={STATUS_LABEL[status]}
                              />
                            </span>
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </section>
              ))}
            </div>
          )}
        </div>

        <div className="flex items-center justify-between text-xs text-white/50">
          <span aria-live="polite">
            {total === 0 ? 'No hits' : `${offset + 1}–${pageEnd} of ${total}`}
          </span>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => setOffset((value) => Math.max(0, value - PAGE_SIZE))}
              disabled={!hasPrevious || loading}
              className="inline-flex items-center gap-1 rounded border border-white/10 px-2 py-1 hover:bg-white/5 disabled:cursor-not-allowed disabled:opacity-40"
            >
              <ChevronLeft aria-hidden className="size-3.5" /> Previous
            </button>
            <button
              type="button"
              onClick={() => setOffset((value) => value + PAGE_SIZE)}
              disabled={!hasNext || loading}
              className="inline-flex items-center gap-1 rounded border border-white/10 px-2 py-1 hover:bg-white/5 disabled:cursor-not-allowed disabled:opacity-40"
            >
              Next <ChevronRight aria-hidden className="size-3.5" />
            </button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
