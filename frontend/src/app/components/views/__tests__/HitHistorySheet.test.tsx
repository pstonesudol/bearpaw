import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HitHistorySheet } from '../HitHistorySheet';
import { getHits } from '../../../../api/recordings';
import type { HitDetail } from '../../../../types';

vi.mock('../../../../api/recordings', () => ({
  getHits: vi.fn(),
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
