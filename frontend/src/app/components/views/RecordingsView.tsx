import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Check,
  Loader2,
  Mic,
  Pause,
  Pencil,
  Play,
  RefreshCw,
  Search,
  Trash2,
  X,
} from 'lucide-react';
import { toast } from 'sonner';
import { cn } from '../../../lib/utils';
import {
  deleteRecording,
  editTranscript,
  getHitWaveform,
  getHits,
  getRecordingsSummary,
  hitAudioUrl,
  transcribeHit,
  type HitQuery,
} from '../../../api/recordings';
import type { HitDetail, HitWaveform, RecordingsSummary, TranscriptStatus } from '../../../types';
import { HitDetailSheet, formatDuration } from './HitDetailSheet';

export interface RecordingsViewProps {
  /** Opens the full hit sheet (playback, transcript history, delete). */
  onOpenHit?: (id: string) => void;
}

const PAGE_SIZE = 50;
const WAVEFORM_BUCKETS = 32;

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

type QuickFilter = 'all' | 'audio' | 'untranscribed' | 'failed';
type GroupBy = 'day' | 'channel' | 'none';
type SortOrder = 'newest' | 'oldest';

function hitId(hit: HitDetail): string {
  return hit.hit_uid ?? hit.id;
}

function hasAudio(hit: HitDetail): boolean {
  return hit.recording_status === 'ready' || hit.recording_status === 'partial';
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 KB';
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

/** `1:04`, for a player readout rather than a duration label. */
function formatClock(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
  const whole = Math.floor(seconds);
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`;
}

function formatAge(timestamp: number): string {
  const seconds = Math.max(0, Date.now() / 1000 - timestamp);
  if (seconds < 90) return 'just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

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

interface Group {
  key: string;
  label: string;
  hits: HitDetail[];
  audioSeconds: number;
  bytes: number;
}

/**
 * Group the rows the user asked to see together.
 *
 * The list arrives sorted, so a single pass preserves the sort inside each
 * group. Totals are of the rows in view, and the header reports the page
 * count separately.
 */
function buildGroups(hits: HitDetail[], groupBy: GroupBy): Group[] {
  if (groupBy === 'none') {
    return [{ key: 'all', label: '', hits, audioSeconds: 0, bytes: 0 }];
  }
  const groups: Group[] = [];
  for (const hit of hits) {
    const key =
      groupBy === 'day'
        ? (() => {
            const date = new Date(hit.timestamp * 1000);
            return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
          })()
        : hit.channel != null
          ? `ch-${hit.channel}`
          : 'no-channel';
    let group = groups[groups.length - 1];
    if (!group || group.key !== key) {
      const date = new Date(hit.timestamp * 1000);
      group = {
        key,
        label:
          groupBy === 'day'
            ? date.toLocaleDateString(undefined, {
                weekday: 'short',
                month: 'short',
                day: 'numeric',
              })
            : hit.channel != null
              ? `CH ${hit.channel}${hit.alpha_tag ? ` · ${hit.alpha_tag}` : ''}`
              : 'No channel',
        hits: [],
        audioSeconds: 0,
        bytes: 0,
      };
      groups.push(group);
    }
    group.hits.push(hit);
    if (hasAudio(hit)) {
      group.audioSeconds += hit.recording_duration ?? hit.duration ?? 0;
      group.bytes += hit.recording_bytes ?? 0;
    }
  }
  return groups;
}

/** Waveform drawn from peaks the backend measured, not from a guess. */
function WaveformBars({
  waveform,
  onMount,
  className,
}: {
  waveform: HitWaveform | null | undefined;
  onMount?: () => void;
  className?: string;
}) {
  // Ask for the peaks once, when the row mounts. The parent dedupes and caches,
  // so scrolling the same rows back into view costs nothing.
  const requested = useRef(false);
  useEffect(() => {
    if (requested.current) return;
    requested.current = true;
    onMount?.();
  }, [onMount]);

  const buckets = waveform?.buckets ?? [];
  return (
    <span
      aria-hidden
      className={cn('flex h-4 w-20 shrink-0 items-end gap-px overflow-hidden', className)}
    >
      {buckets.map((value, index) => (
        <i
          key={index}
          style={{ height: `${Math.max(8, value)}%` }}
          className={cn(
            'min-w-px flex-1 rounded-sm',
            waveform?.clipped ? 'bg-red-400/80' : 'bg-brand-primary/80',
          )}
        />
      ))}
    </span>
  );
}

export function RecordingsView({ onOpenHit }: RecordingsViewProps) {
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [channel, setChannel] = useState('');
  const [quick, setQuick] = useState<QuickFilter>('all');
  const [groupBy, setGroupBy] = useState<GroupBy>('day');
  const [order, setOrder] = useState<SortOrder>('newest');

  const [rows, setRows] = useState<HitDetail[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [summary, setSummary] = useState<RecordingsSummary | null>(null);
  const [waves, setWaves] = useState<Record<string, HitWaveform>>({});
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [playing, setPlaying] = useState(false);
  const [time, setTime] = useState(0);
  const [audioDuration, setAudioDuration] = useState(0);
  const [playError, setPlayError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [correctingId, setCorrectingId] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [retryingFailed, setRetryingFailed] = useState(false);
  const [detailId, setDetailId] = useState<string | null>(null);

  const audioRef = useRef<HTMLAudioElement | null>(null);
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());
  const requestedWaves = useRef(new Set<string>());
  const pageRequest = useRef(0);
  const sentinelRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const timer = window.setTimeout(() => setDebouncedSearch(search.trim()), 300);
    return () => window.clearTimeout(timer);
  }, [search]);

  const query = useMemo<HitQuery>(() => {
    const channelNumber = Number.parseInt(channel, 10);
    const hasRecording = quick === 'audio' || quick === 'untranscribed' ? true : undefined;
    const transcriptStatus =
      quick === 'untranscribed' ? 'none' : quick === 'failed' ? 'failed' : undefined;
    return {
      search: debouncedSearch || undefined,
      channel: Number.isFinite(channelNumber) ? channelNumber : undefined,
      start_time: dayStart(from),
      end_time: dayEnd(to),
      has_recording: hasRecording,
      transcript_status: transcriptStatus,
      sort: 'timestamp',
      order: order === 'oldest' ? 'asc' : 'desc',
    };
  }, [debouncedSearch, channel, from, to, quick, order]);

  const loadFirstPage = useCallback(async () => {
    const request = ++pageRequest.current;
    setLoading(true);
    try {
      const page = await getHits({ ...query, limit: PAGE_SIZE, offset: 0 });
      if (request !== pageRequest.current) return;
      setRows(page.hits);
      setTotal(page.total);
      setError(null);
      setSelectedId(page.hits.length > 0 ? hitId(page.hits[0]) : null);
    } catch (e) {
      if (request !== pageRequest.current) return;
      setError(e instanceof Error ? e.message : 'Could not load recordings');
    } finally {
      if (request === pageRequest.current) setLoading(false);
    }
  }, [query]);

  const loadMore = useCallback(async () => {
    if (loading || loadingMore || rows.length >= total) return;
    const request = pageRequest.current;
    setLoadingMore(true);
    try {
      const page = await getHits({ ...query, limit: PAGE_SIZE, offset: rows.length });
      if (request !== pageRequest.current) return;
      setRows((previous) => {
        const seen = new Set(previous.map(hitId));
        return [...previous, ...page.hits.filter((hit) => !seen.has(hitId(hit)))];
      });
      setTotal(page.total);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not load more recordings');
    } finally {
      if (request === pageRequest.current) setLoadingMore(false);
    }
  }, [loading, loadingMore, query, rows.length, total]);

  const loadSummary = useCallback(async () => {
    try {
      setSummary(await getRecordingsSummary());
    } catch {
      setSummary(null);
    }
  }, []);

  // Data-fetch lifecycle, not derived state: the loaders set state before they
  // await, so firing them when the query changes is the only trigger available.
  useEffect(() => {
    // Filter/sort change: start over from the newest row.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void loadFirstPage();
  }, [loadFirstPage]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void loadSummary();
  }, [loadSummary]);

  // Continuous scroll: the sentinel is the last thing in the list, so reaching
  // it is the same signal as "the user has read what is loaded".
  useEffect(() => {
    if (loading || loadingMore || rows.length >= total) return;
    const sentinel = sentinelRef.current;
    if (!sentinel || typeof IntersectionObserver === 'undefined') return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void loadMore();
      },
      { rootMargin: '240px' },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loading, loadingMore, loadMore, rows.length, total]);

  // Closing the tab stops playback: the audio belongs to this view.
  useEffect(
    () => () => {
      audioRef.current?.pause();
    },
    [],
  );

  const loadWaveform = useCallback((id: string) => {
    if (!id || requestedWaves.current.has(id)) return;
    requestedWaves.current.add(id);
    getHitWaveform(id, WAVEFORM_BUCKETS)
      .then((waveform) => setWaves((previous) => ({ ...previous, [id]: waveform })))
      .catch(() => {
        requestedWaves.current.delete(id);
      });
  }, []);

  const hitById = useCallback(
    (id: string | null) => (id ? rows.find((row) => hitId(row) === id) : undefined),
    [rows],
  );

  const startPlayback = useCallback(
    (hit: HitDetail) => {
      const id = hitId(hit);
      const audio = audioRef.current;
      if (!audio) return;
      if (currentId !== id) {
        audio.src = hitAudioUrl(id);
        setCurrentId(id);
        setTime(0);
        setAudioDuration(hit.recording_duration ?? hit.duration ?? 0);
      }
      setExpandedId(id);
      setPlayError(null);
      setPlaying(true);
      const started = audio.play();
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
      startPlayback(hit);
    },
    [currentId, playing, startPlayback],
  );

  const handleEnded = () => {
    setPlaying(false);
    const order_ = rows.filter(hasAudio).map(hitId);
    const index = currentId ? order_.indexOf(currentId) : -1;
    const next = index >= 0 ? order_[index + 1] : undefined;
    if (next) {
      const hit = hitById(next);
      if (hit) startPlayback(hit);
    }
  };

  const seek = (value: number) => {
    const audio = audioRef.current;
    if (!audio) return;
    audio.currentTime = value;
    setTime(value);
  };

  const retryTranscription = useCallback(
    async (hit: HitDetail) => {
      const id = hitId(hit);
      setBusyId(id);
      try {
        await transcribeHit(id, true);
        setRows((previous) =>
          previous.map((row) =>
            hitId(row) === id
              ? { ...row, transcript_status: 'pending', transcript_error: null }
              : row,
          ),
        );
        toast.success('Transcription queued again');
        await loadSummary();
      } catch (e) {
        toast.error(e instanceof Error ? e.message : 'Could not queue transcription');
      } finally {
        setBusyId(null);
      }
    },
    [loadSummary],
  );

  const startCorrect = (hit: HitDetail) => {
    setExpandedId(hitId(hit));
    setCorrectingId(hitId(hit));
    setDraft(hit.transcript ?? '');
  };

  const saveCorrection = async (hit: HitDetail) => {
    const id = hitId(hit);
    setBusyId(id);
    try {
      const updated = await editTranscript(id, draft);
      setRows((previous) =>
        previous.map((row) => (hitId(row) === id ? { ...row, ...updated } : row)),
      );
      setCorrectingId(null);
      toast.success('Transcript corrected');
      await loadSummary();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not save the transcript');
    } finally {
      setBusyId(null);
    }
  };

  const removeRecording = async (hit: HitDetail) => {
    const id = hitId(hit);
    setBusyId(id);
    try {
      await deleteRecording(id);
      if (currentId === id) {
        audioRef.current?.pause();
        setPlaying(false);
        setCurrentId(null);
      }
      setRows((previous) =>
        previous.map((row) =>
          hitId(row) === id
            ? {
                ...row,
                recording_status: 'deleted',
                recording_file: null,
                transcript: null,
                transcript_status: 'none',
              }
            : row,
        ),
      );
      setConfirmDeleteId(null);
      toast.success('Recording and transcript deleted. The hit stays in the list.');
      await loadSummary();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not delete the recording');
    } finally {
      setBusyId(null);
    }
  };

  const retryAllFailed = async () => {
    setRetryingFailed(true);
    try {
      const page = await getHits({
        transcript_status: 'failed',
        has_recording: true,
        limit: 100,
      });
      const failed = page.hits.filter(hasAudio);
      if (failed.length === 0) {
        toast.info('No failed transcripts to retry');
        return;
      }
      await Promise.all(failed.map((hit) => transcribeHit(hitId(hit), true)));
      toast.success(`${failed.length} transcription${failed.length === 1 ? '' : 's'} queued`);
      await Promise.all([loadFirstPage(), loadSummary()]);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Could not retry transcriptions');
    } finally {
      setRetryingFailed(false);
    }
  };

  const focusRow = (id: string) => {
    setSelectedId(id);
    rowRefs.current.get(id)?.focus();
  };

  const handleListKeyDown = (event: React.KeyboardEvent) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const target = event.target as HTMLElement | null;
    if (
      target &&
      (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'SELECT')
    ) {
      return;
    }
    const ids = rows.map(hitId);
    if (ids.length === 0) return;
    const index = selectedId ? ids.indexOf(selectedId) : -1;

    if (event.key === 'j' || event.key === 'J' || event.key === 'ArrowDown') {
      event.preventDefault();
      focusRow(ids[Math.min(ids.length - 1, index + 1)]);
      return;
    }
    if (event.key === 'k' || event.key === 'K' || event.key === 'ArrowUp') {
      event.preventDefault();
      focusRow(ids[Math.max(0, index <= 0 ? 0 : index - 1)]);
      return;
    }

    const hit = hitById(selectedId);
    if (!hit) return;
    if (event.key === ' ') {
      event.preventDefault();
      togglePlay(hit);
    } else if (event.key === 't' || event.key === 'T') {
      event.preventDefault();
      void retryTranscription(hit);
    } else if (event.key === 'c' || event.key === 'C') {
      event.preventDefault();
      startCorrect(hit);
    } else if (event.key === 'd' || event.key === 'D') {
      event.preventDefault();
      setExpandedId(hitId(hit));
      setConfirmDeleteId(hitId(hit));
    }
  };

  const groups = useMemo(() => buildGroups(rows, groupBy), [rows, groupBy]);
  const hasMore = rows.length < total;

  const quickFilters: Array<{ id: QuickFilter; label: string; count?: number }> = [
    { id: 'all', label: 'All', count: summary?.hits_total },
    { id: 'audio', label: 'With audio', count: summary?.count },
    { id: 'untranscribed', label: 'Untranscribed', count: summary?.untranscribed },
    { id: 'failed', label: 'Failed', count: summary?.failed_transcripts },
  ];

  return (
    <div className="flex h-full flex-col gap-3">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h2 className="text-xl font-bold text-scanner-text-light">Recordings</h2>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-white/45">
            {summary ? (
              <>
                <span className="text-white/70">
                  {summary.count} {summary.count === 1 ? 'file' : 'files'}
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
              </>
            ) : (
              <span>Library totals unavailable</span>
            )}
          </p>
        </div>
        {summary && summary.failed_transcripts > 0 && (
          <button
            type="button"
            onClick={() => void retryAllFailed()}
            disabled={retryingFailed}
            className="inline-flex shrink-0 items-center gap-1.5 rounded border border-white/10 px-2.5 py-1 text-xs text-white/80 hover:bg-white/5 disabled:cursor-not-allowed disabled:opacity-50"
          >
            <RefreshCw aria-hidden className={cn('size-3.5', retryingFailed && 'animate-spin')} />
            Retry {summary.failed_transcripts} failed
          </button>
        )}
      </header>

      <div className="flex flex-wrap items-center gap-2">
        {quickFilters.map((filter) => (
          <button
            key={filter.id}
            type="button"
            aria-pressed={quick === filter.id}
            onClick={() => setQuick(filter.id)}
            className={cn(
              'rounded-full border px-3 py-1 text-xs transition-colors',
              quick === filter.id
                ? 'border-brand-primary/50 bg-brand-primary/15 text-brand-light'
                : 'border-white/10 bg-white/[0.03] text-white/55 hover:bg-white/5',
            )}
          >
            {filter.label}
            {filter.count != null && filter.count > 0 && (
              <span className="ml-1.5 text-white/40 tabular-nums">{filter.count}</span>
            )}
          </button>
        ))}
      </div>

      <div className="flex flex-wrap items-end gap-2">
        <label className="flex min-w-[14rem] flex-1 items-center gap-2 rounded border border-white/15 bg-black/30 px-2">
          <Search aria-hidden className="size-4 shrink-0 text-white/40" />
          <span className="sr-only">Search tag, frequency or transcript</span>
          <input
            type="search"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search transcript, tag or frequency…"
            className="w-full bg-transparent py-2 text-sm text-white placeholder:text-white/35 focus:outline-none"
          />
        </label>
        <label className="text-xs text-white/50">
          From
          <input
            type="date"
            value={from}
            onChange={(event) => setFrom(event.target.value)}
            className="mt-1 w-32 rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
          />
        </label>
        <label className="text-xs text-white/50">
          To
          <input
            type="date"
            value={to}
            onChange={(event) => setTo(event.target.value)}
            className="mt-1 w-32 rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
          />
        </label>
        <label className="text-xs text-white/50">
          Channel
          <input
            inputMode="numeric"
            value={channel}
            onChange={(event) => setChannel(event.target.value.replace(/[^0-9]/g, ''))}
            placeholder="Any"
            className="mt-1 w-20 rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
          />
        </label>
        <label className="text-xs text-white/50">
          Group
          <select
            value={groupBy}
            onChange={(event) => setGroupBy(event.target.value as GroupBy)}
            className="mt-1 rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
          >
            <option value="day" className="bg-[#12151b]">
              Day
            </option>
            <option value="channel" className="bg-[#12151b]">
              Channel
            </option>
            <option value="none" className="bg-[#12151b]">
              None
            </option>
          </select>
        </label>
        <label className="text-xs text-white/50">
          Sort
          <select
            value={order}
            onChange={(event) => setOrder(event.target.value as SortOrder)}
            className="mt-1 rounded border border-white/15 bg-black/30 px-2 py-1.5 text-sm text-white"
          >
            <option value="newest" className="bg-[#12151b]">
              Newest first
            </option>
            <option value="oldest" className="bg-[#12151b]">
              Oldest first
            </option>
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
        data-testid="recordings-audio"
        preload="none"
        onEnded={handleEnded}
        onTimeUpdate={(event) => setTime(event.currentTarget.currentTime)}
        onLoadedMetadata={(event) => {
          const duration = event.currentTarget.duration;
          if (Number.isFinite(duration) && duration > 0) setAudioDuration(duration);
        }}
        onError={() => {
          setPlayError('Could not play this recording.');
          setPlaying(false);
        }}
        className="hidden"
      />

      <div
        ref={sentinelRef}
        role="list"
        aria-label="Recordings"
        aria-busy={loading}
        tabIndex={0}
        onKeyDown={handleListKeyDown}
        className="min-h-[220px] flex-1 overflow-y-auto rounded border border-white/10 focus:outline-none"
      >
        {loading && rows.length === 0 ? (
          <div className="flex items-center justify-center gap-2 py-16 text-sm text-white/60">
            <Loader2 aria-hidden className="size-4 animate-spin" /> Loading recordings…
          </div>
        ) : error ? (
          <div className="space-y-2 py-10 text-center text-sm">
            <p role="alert" className="text-red-300">
              {error}
            </p>
            <button
              type="button"
              onClick={() => void loadFirstPage()}
              className="rounded border border-white/10 px-3 py-1.5 text-white/70 hover:bg-white/5"
            >
              Try again
            </button>
          </div>
        ) : rows.length === 0 ? (
          <div className="py-16 text-center text-sm text-white/50">
            No recordings match these filters.
          </div>
        ) : (
          <div>
            {groups.map((group) => (
              <section key={group.key} aria-label={group.label || undefined}>
                {group.label && (
                  <h3 className="sticky top-0 z-10 flex items-baseline justify-between gap-2 border-b border-white/5 bg-[#151920]/95 px-3 py-1.5 backdrop-blur">
                    <span className="text-[0.65rem] font-semibold tracking-[0.14em] text-white/70 uppercase">
                      {group.label}
                    </span>
                    <span className="text-[0.7rem] text-white/40 tabular-nums">
                      {groupBy === 'none'
                        ? ''
                        : `${group.hits.length} ${group.hits.length === 1 ? 'hit' : 'hits'}${
                            group.audioSeconds > 0 ? ` · ${formatDuration(group.audioSeconds)}` : ''
                          }`}
                    </span>
                  </h3>
                )}
                <ul className="divide-y divide-white/5">
                  {group.hits.map((hit) => {
                    const id = hitId(hit);
                    const status = (hit.transcript_status ?? 'none') as TranscriptStatus;
                    const playable = hasAudio(hit);
                    const isPlaying = playing && currentId === id;
                    const expanded = expandedId === id;
                    const waveform = waves[id];
                    const duration = hit.recording_duration ?? hit.duration ?? 0;
                    return (
                      <li
                        key={id}
                        role="listitem"
                        className={cn(
                          'flex flex-wrap items-center gap-2 py-1.5 pr-3 pl-2',
                          expanded && 'bg-brand-primary/[0.07]',
                          isPlaying && 'ring-1 ring-inset ring-brand-primary/30',
                        )}
                      >
                        {playable ? (
                          <button
                            type="button"
                            onClick={() => togglePlay(hit)}
                            aria-pressed={isPlaying}
                            aria-label={`${isPlaying ? 'Pause' : 'Play'} recording at ${hit.frequency.toFixed(4)} MHz`}
                            className={cn(
                              'grid size-8 shrink-0 place-items-center rounded-full border transition-colors',
                              isPlaying
                                ? 'border-brand-primary bg-brand-primary text-[#12151b]'
                                : 'border-white/15 text-brand-light hover:border-brand-primary/60 hover:bg-white/5',
                            )}
                          >
                            {isPlaying ? (
                              <Pause aria-hidden className="size-4" />
                            ) : (
                              <Play aria-hidden className="size-4" />
                            )}
                          </button>
                        ) : (
                          <span className="size-8 shrink-0" aria-hidden />
                        )}

                        <button
                          type="button"
                          ref={(element) => {
                            if (element) rowRefs.current.set(id, element);
                            else rowRefs.current.delete(id);
                          }}
                          onClick={() => {
                            if (onOpenHit) onOpenHit(id);
                            else setDetailId(id);
                          }}
                          onFocus={() => setSelectedId(id)}
                          aria-current={selectedId === id ? 'true' : undefined}
                          aria-label={`Open hit at ${hit.frequency.toFixed(4)} MHz${
                            hit.alpha_tag ? `, ${hit.alpha_tag}` : ''
                          }`}
                          className={cn(
                            'grid min-w-0 flex-1 grid-cols-[4.2rem_5.4rem_1fr_5rem_5.5rem] items-center gap-3 rounded px-1 py-1 text-left text-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-brand-primary/60',
                            selectedId === id && 'bg-white/[0.04]',
                          )}
                        >
                          <span className="text-xs text-white/50 tabular-nums">
                            {new Date(hit.timestamp * 1000).toLocaleTimeString(undefined, {
                              hour: '2-digit',
                              minute: '2-digit',
                              second: '2-digit',
                              hourCycle: 'h23',
                            })}
                          </span>
                          <span className="font-mono text-xs text-brand-light tabular-nums">
                            {hit.frequency.toFixed(4)}
                          </span>
                          <span className="min-w-0">
                            <span className="block truncate text-white/85">
                              {hit.alpha_tag || '—'}
                              {hit.channel != null && (
                                <span className="ml-2 text-xs text-white/35">ch {hit.channel}</span>
                              )}
                            </span>
                            {correctingId === id ? null : hit.transcript ? (
                              <span className="block truncate text-xs text-white/45">
                                {hit.transcript}
                              </span>
                            ) : status === 'failed' ? (
                              <span className="block truncate text-xs text-red-300/70">
                                Transcription failed
                              </span>
                            ) : status === 'pending' || status === 'processing' ? (
                              <span className="block truncate text-xs text-amber-200/70">
                                {STATUS_LABEL[status]}…
                              </span>
                            ) : null}
                          </span>
                          <WaveformBars
                            waveform={waveform}
                            onMount={() => loadWaveform(id)}
                            className="justify-self-end"
                          />
                          <span className="flex items-center justify-end gap-2 text-xs text-white/50 tabular-nums">
                            <span title={formatDuration(duration)}>{formatDuration(duration)}</span>
                            {playable ? (
                              <Mic
                                aria-label="Has recording"
                                className="size-3.5 shrink-0 text-brand-primary"
                              />
                            ) : (
                              <span className="size-3.5 shrink-0" aria-hidden />
                            )}
                            <span
                              className={cn('size-2 shrink-0 rounded-full', statusDot[status])}
                              aria-label={STATUS_LABEL[status]}
                            />
                          </span>
                        </button>

                        {expanded && (
                          <div className="basis-full border-t border-white/5 pt-2 pb-1 pl-10">
                            {correctingId === id ? (
                              <div className="space-y-2">
                                <label
                                  htmlFor={`transcript-${id}`}
                                  className="text-xs text-white/50"
                                >
                                  Transcript
                                </label>
                                <textarea
                                  id={`transcript-${id}`}
                                  value={draft}
                                  onChange={(event) => setDraft(event.target.value)}
                                  rows={3}
                                  className="w-full rounded border border-white/15 bg-black/40 px-2 py-1.5 text-sm text-white"
                                />
                                <div className="flex items-center gap-2">
                                  <button
                                    type="button"
                                    disabled={busyId === id}
                                    onClick={() => void saveCorrection(hit)}
                                    className="inline-flex items-center gap-1 rounded border border-brand-primary/50 bg-brand-primary/15 px-2.5 py-1 text-xs text-brand-light disabled:opacity-50"
                                  >
                                    <Check aria-hidden className="size-3.5" /> Save
                                  </button>
                                  <button
                                    type="button"
                                    onClick={() => setCorrectingId(null)}
                                    className="inline-flex items-center gap-1 rounded border border-white/10 px-2.5 py-1 text-xs text-white/70 hover:bg-white/5"
                                  >
                                    <X aria-hidden className="size-3.5" /> Cancel
                                  </button>
                                  {hit.transcript_original && (
                                    <span className="ml-auto text-xs text-white/35">
                                      Machine: {hit.transcript_original}
                                    </span>
                                  )}
                                </div>
                              </div>
                            ) : (
                              <div className="flex flex-wrap items-center gap-3">
                                <div className="flex min-w-[12rem] flex-1 items-center gap-2">
                                  <span className="w-9 text-right text-xs text-white/50 tabular-nums">
                                    {formatClock(currentId === id ? time : 0)}
                                  </span>
                                  <input
                                    type="range"
                                    min={0}
                                    max={Math.max(audioDuration || duration || 0.1, 0.1)}
                                    step={0.01}
                                    value={Math.min(time, audioDuration || duration || 0)}
                                    disabled={currentId !== id}
                                    onChange={(event) => seek(Number(event.target.value))}
                                    aria-label="Seek within the recording"
                                    className="h-1 flex-1 cursor-pointer appearance-none rounded bg-white/15 accent-brand-primary disabled:opacity-40"
                                  />
                                  <span className="w-10 text-xs text-white/50 tabular-nums">
                                    {formatDuration(audioDuration || duration)}
                                  </span>
                                </div>
                                <div className="flex flex-wrap items-center gap-2">
                                  <button
                                    type="button"
                                    disabled={busyId === id || !playable}
                                    onClick={() => void retryTranscription(hit)}
                                    className="inline-flex items-center gap-1 rounded border border-brand-primary/50 bg-brand-primary/15 px-2.5 py-1 text-xs text-brand-light disabled:cursor-not-allowed disabled:opacity-40"
                                  >
                                    <RefreshCw
                                      aria-hidden
                                      className={cn('size-3.5', busyId === id && 'animate-spin')}
                                    />
                                    Retry transcript
                                  </button>
                                  <button
                                    type="button"
                                    onClick={() => startCorrect(hit)}
                                    className="inline-flex items-center gap-1 rounded border border-white/10 px-2.5 py-1 text-xs text-white/80 hover:bg-white/5"
                                  >
                                    <Pencil aria-hidden className="size-3.5" /> Correct
                                  </button>
                                  {confirmDeleteId === id ? (
                                    <>
                                      <button
                                        type="button"
                                        disabled={busyId === id}
                                        onClick={() => void removeRecording(hit)}
                                        className="inline-flex items-center gap-1 rounded border border-red-400/40 bg-red-500/10 px-2.5 py-1 text-xs text-red-200 disabled:opacity-50"
                                      >
                                        <Trash2 aria-hidden className="size-3.5" /> Delete for good
                                      </button>
                                      <button
                                        type="button"
                                        onClick={() => setConfirmDeleteId(null)}
                                        className="rounded border border-white/10 px-2.5 py-1 text-xs text-white/70 hover:bg-white/5"
                                      >
                                        Cancel
                                      </button>
                                    </>
                                  ) : (
                                    <button
                                      type="button"
                                      onClick={() => setConfirmDeleteId(id)}
                                      disabled={!playable}
                                      className="inline-flex items-center gap-1 rounded border border-white/10 px-2.5 py-1 text-xs text-white/70 hover:bg-white/5 disabled:cursor-not-allowed disabled:opacity-40"
                                    >
                                      <Trash2 aria-hidden className="size-3.5" /> Delete
                                    </button>
                                  )}
                                  {waveform?.clipped && (
                                    <span className="text-xs text-red-300/80">
                                      peak {waveform.peak_dbfs.toFixed(1)} dBFS · clipped
                                    </span>
                                  )}
                                </div>
                              </div>
                            )}
                          </div>
                        )}
                      </li>
                    );
                  })}
                </ul>
              </section>
            ))}

            <div className="flex items-center justify-center gap-2 py-3 text-xs text-white/45">
              {loadingMore ? (
                <>
                  <Loader2 aria-hidden className="size-3.5 animate-spin" /> Loading older…
                </>
              ) : hasMore ? (
                <button
                  type="button"
                  onClick={() => void loadMore()}
                  className="rounded border border-white/10 px-3 py-1 hover:bg-white/5"
                >
                  Load older recordings
                </button>
              ) : (
                <span>
                  {rows.length} of {total} shown · end of the list
                </span>
              )}
            </div>
          </div>
        )}
      </div>

      <footer className="flex flex-wrap items-center justify-between gap-2 text-[0.7rem] text-white/40">
        <span aria-live="polite">
          {total === 0 ? 'No recordings' : `${rows.length} of ${total} loaded`}
        </span>
        <span className="flex flex-wrap items-center gap-1">
          <kbd className="rounded border border-white/15 px-1">space</kbd> play
          <span aria-hidden>·</span>
          <kbd className="rounded border border-white/15 px-1">J</kbd>
          <kbd className="rounded border border-white/15 px-1">K</kbd> move
          <span aria-hidden>·</span>
          <kbd className="rounded border border-white/15 px-1">T</kbd> transcribe
          <span aria-hidden>·</span>
          <kbd className="rounded border border-white/15 px-1">C</kbd> correct
          <span aria-hidden>·</span>
          <kbd className="rounded border border-white/15 px-1">D</kbd> delete
        </span>
      </footer>

      <HitDetailSheet hitUid={detailId} initial={null} onClose={() => setDetailId(null)} />
    </div>
  );
}
