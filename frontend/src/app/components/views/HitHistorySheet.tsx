import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, ChevronRight, Loader2, Mic, Search } from 'lucide-react';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { cn } from '../../../lib/utils';
import { getHits, type HitQuery } from '../../../api/recordings';
import type { HitDetail, TranscriptStatus } from '../../../types';
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

const statusDot: Record<TranscriptStatus, string> = {
  none: 'bg-white/20',
  pending: 'bg-amber-400',
  processing: 'bg-amber-400 animate-pulse',
  completed: 'bg-emerald-400',
  failed: 'bg-red-400',
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

export function HitHistorySheet({ isOpen, onClose, onSelectHit }: HitHistorySheetProps) {
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [recording, setRecording] = useState<'any' | 'yes' | 'no'>('any');
  const [transcriptStatus, setTranscriptStatus] = useState('any');
  const [channel, setChannel] = useState('');
  const [offset, setOffset] = useState(0);
  const [hits, setHits] = useState<HitDetail[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
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

  useEffect(() => {
    if (!isOpen) return;
    // Data-fetch lifecycle, not derived state: `load` sets `loading` before it
    // awaits. Firing it when the dialog opens is the only trigger available.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void load();
  }, [isOpen, load]);

  const pageEnd = Math.min(offset + hits.length, total);
  const hasPrevious = offset > 0;
  const hasNext = offset + PAGE_SIZE < total;

  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-4xl border-white/10 bg-[#12151b] text-white">
        <DialogTitle className="pr-8 text-xl font-bold">Hit History</DialogTitle>

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
                setRecording(event.target.value as 'any' | 'yes' | 'no');
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

        <div
          className="min-h-[240px] max-h-[50vh] overflow-y-auto rounded border border-white/10"
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
            <ul className="divide-y divide-white/5">
              {hits.map((hit) => {
                const id = hit.hit_uid ?? hit.id;
                const status = (hit.transcript_status ?? 'none') as TranscriptStatus;
                return (
                  <li key={id}>
                    <button
                      type="button"
                      onClick={() => onSelectHit(id)}
                      className="grid w-full grid-cols-[9rem_6.5rem_1fr_auto] items-center gap-3 px-3 py-2 text-left text-sm hover:bg-white/5 focus:bg-white/5 focus:outline-none"
                      aria-label={`Open hit at ${hit.frequency.toFixed(4)} MHz${
                        hit.alpha_tag ? `, ${hit.alpha_tag}` : ''
                      }`}
                    >
                      <span className="text-xs text-white/50">
                        {new Date(hit.timestamp * 1000).toLocaleString(undefined, {
                          month: 'short',
                          day: 'numeric',
                          hour: '2-digit',
                          minute: '2-digit',
                          second: '2-digit',
                        })}
                      </span>
                      <span className="font-mono text-brand-light">{hit.frequency.toFixed(4)}</span>
                      <span className="min-w-0">
                        <span className="block truncate text-white/80">
                          {hit.alpha_tag || '—'}
                          <span className="ml-2 text-xs text-white/35">
                            {hit.channel != null ? `ch ${hit.channel}` : ''}
                          </span>
                        </span>
                        {hit.transcript && (
                          <span className="block truncate text-xs text-white/45">
                            {hit.transcript}
                          </span>
                        )}
                      </span>
                      <span className="flex items-center gap-2 text-xs text-white/50">
                        <span title={formatDuration(hit.duration)} className="tabular-nums">
                          {formatDuration(hit.duration)}
                        </span>
                        {hasAudio(hit) ? (
                          <Mic aria-label="Has recording" className="size-3.5 text-brand-primary" />
                        ) : (
                          <span className="size-3.5" aria-hidden />
                        )}
                        <span
                          className={cn('size-2 rounded-full', statusDot[status])}
                          aria-label={
                            status === 'completed'
                              ? 'Transcribed'
                              : status === 'failed'
                                ? 'Transcription failed'
                                : status === 'none'
                                  ? 'Not transcribed'
                                  : 'Transcription in progress'
                          }
                        />
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
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
