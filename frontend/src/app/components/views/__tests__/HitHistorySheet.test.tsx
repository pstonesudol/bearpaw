import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HitHistorySheet } from '../HitHistorySheet';
import { getHits, getRecordingsSummary, transcribeHit } from '../../../../api/recordings';
import type { HitDetail } from '../../../../types';

vi.mock('../../../../api/recordings', () => ({
  getHits: vi.fn(),
  getRecordingsSummary: vi.fn(),
  transcribeHit: vi.fn(),
  hitAudioUrl: (id: string) => `/api/v1/hits/${id}/audio`,
}));

vi.mock('sonner', () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const hit = (overrides: Partial<HitDetail> = {}): HitDetail => ({
  id: '1',
  timestamp: 1_700_000_000,
  frequency: 160.725,
  channel: 12,
  alpha_tag: 'NORFOLK SOUTHERN',
  duration: 3.5,
  hit_uid: 'h-1',
  recording_status: 'ready',
  transcript_status: 'completed',
  transcript: 'NS 24 clear at Phoenix',
  ...overrides,
});

const dayLabel = (timestamp: number) =>
  new Date(timestamp * 1000).toLocaleDateString(undefined, {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
  });

describe('HitHistorySheet', () => {
  const onSelectHit = vi.fn();
  const onClose = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit()],
      total: 1,
      limit: 25,
      offset: 0,
    });
    vi.mocked(getRecordingsSummary).mockResolvedValue({
      count: 1,
      bytes: 2_048,
      oldest_timestamp: 1_700_000_000,
      failed_transcripts: 0,
      retention_days: 30,
    });
    vi.mocked(transcribeHit).mockResolvedValue({ transcript_status: 'pending' });
    // jsdom has no media pipeline; only the calls matter here.
    vi.spyOn(window.HTMLMediaElement.prototype, 'pause').mockImplementation(() => {});
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('lists hits with recording and transcript indicators', async () => {
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);

    expect(await screen.findByText('NORFOLK SOUTHERN')).toBeInTheDocument();
    expect(screen.getByText('160.7250')).toBeInTheDocument();
    expect(screen.getByText('NS 24 clear at Phoenix')).toBeInTheDocument();
    expect(screen.getByLabelText('Has recording')).toBeInTheDocument();
    expect(screen.getByLabelText('Transcribed')).toBeInTheDocument();
    expect(screen.getByText('1–1 of 1')).toBeInTheDocument();
  });

  it('shows how much audio is stored and how long it is kept', async () => {
    vi.mocked(getRecordingsSummary).mockResolvedValue({
      count: 32,
      bytes: 9_437_184,
      oldest_timestamp: Date.now() / 1000 - 3 * 3600,
      failed_transcripts: 0,
      retention_days: 30,
    });
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);

    expect(await screen.findByText('32 recordings')).toBeInTheDocument();
    expect(screen.getByText('9.0 MB')).toBeInTheDocument();
    expect(screen.getByText('keeping 30 days')).toBeInTheDocument();
    expect(screen.getByText(/oldest \d+h ago/)).toBeInTheDocument();
  });

  it('keeps the list usable when the library summary fails', async () => {
    vi.mocked(getRecordingsSummary).mockRejectedValue(new Error('offline'));
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);

    expect(await screen.findByText('NORFOLK SOUTHERN')).toBeInTheDocument();
    expect(screen.queryByText(/recordings ·/)).not.toBeInTheDocument();
  });

  it('groups rows by day with per-day totals', async () => {
    const later = 1_700_100_000;
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit({ id: '2', hit_uid: 'h-2', timestamp: later, alpha_tag: 'SEPTA NORTH' }), hit()],
      total: 2,
      limit: 25,
      offset: 0,
    });
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);

    await screen.findByText('SEPTA NORTH');
    expect(screen.getByText(dayLabel(later))).toBeInTheDocument();
    expect(screen.getByText(dayLabel(1_700_000_000))).toBeInTheDocument();
    // One hit per group, each counting its own audio.
    expect(screen.getAllByText('1 hit · 3.5s')).toHaveLength(2);
  });

  it('plays a recording in the row without opening the hit', async () => {
    const play = vi.spyOn(window.HTMLMediaElement.prototype, 'play').mockResolvedValue(undefined);
    vi.spyOn(window.HTMLMediaElement.prototype, 'pause').mockImplementation(() => {});
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /play recording at 160.7250 mhz/i }));

    expect(play).toHaveBeenCalled();
    expect(onSelectHit).not.toHaveBeenCalled();
    expect(screen.getByTestId('hit-history-audio')).toHaveAttribute(
      'src',
      '/api/v1/hits/h-1/audio',
    );
    expect(
      screen.getByRole('button', { name: /pause recording at 160.7250 mhz/i }),
    ).toHaveAttribute('aria-pressed', 'true');
  });

  it('offers no play control for a hit without audio', async () => {
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit({ recording_status: 'none', transcript_status: 'none', transcript: null })],
      total: 1,
      limit: 25,
      offset: 0,
    });
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    expect(screen.queryByRole('button', { name: /play recording/i })).not.toBeInTheDocument();
  });

  it('queues every failed transcript from the header', async () => {
    vi.mocked(getRecordingsSummary).mockResolvedValue({
      count: 6,
      bytes: 4_096,
      oldest_timestamp: 1_700_000_000,
      failed_transcripts: 6,
      retention_days: 30,
    });
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit({ transcript_status: 'failed', transcript: null })],
      total: 6,
      limit: 25,
      offset: 0,
    });
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /retry 6 failed/i }));
    await waitFor(() => expect(transcribeHit).toHaveBeenCalledWith('h-1', true));
  });

  it('filters by transcript text after a debounce', async () => {
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.type(screen.getByRole('searchbox'), 'amtrak');
    await waitFor(
      () =>
        expect(getHits).toHaveBeenLastCalledWith(
          expect.objectContaining({ search: 'amtrak', offset: 0 }),
        ),
      { timeout: 2000 },
    );
  });

  it('filters by recording availability and transcript status', async () => {
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.selectOptions(screen.getByLabelText('Recording'), 'yes');
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(expect.objectContaining({ has_recording: true })),
    );

    await user.selectOptions(screen.getByLabelText('Transcript'), 'failed');
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(
        expect.objectContaining({ transcript_status: 'failed' }),
      ),
    );
  });

  it('applies a quick filter through the same query', async () => {
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /with audio/i }));
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(
        expect.objectContaining({ has_recording: true, transcript_status: undefined }),
      ),
    );
  });

  it('opens the durable hit, not the numeric row id', async () => {
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /open hit at 160.7250 mhz/i }));
    expect(onSelectHit).toHaveBeenCalledWith('h-1');
  });

  it('falls back to the numeric id for a hit recorded before uid support', async () => {
    vi.mocked(getHits).mockResolvedValue({
      hits: [
        hit({
          hit_uid: null,
          recording_status: 'none',
          transcript_status: 'none',
          transcript: null,
        }),
      ],
      total: 1,
      limit: 25,
      offset: 0,
    });
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /open hit at 160.7250 mhz/i }));
    expect(onSelectHit).toHaveBeenCalledWith('1');
  });

  it('pages forward without losing the filter', async () => {
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit()],
      total: 60,
      limit: 25,
      offset: 0,
    });
    const user = userEvent.setup();
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /next/i }));
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(expect.objectContaining({ offset: 25 })),
    );
  });

  it('says so when nothing matches', async () => {
    vi.mocked(getHits).mockResolvedValue({ hits: [], total: 0, limit: 25, offset: 0 });
    render(<HitHistorySheet isOpen onClose={onClose} onSelectHit={onSelectHit} />);
    expect(await screen.findByText('No hits match these filters.')).toBeInTheDocument();
  });
});
