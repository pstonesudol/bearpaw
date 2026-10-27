import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { RecordingsView } from '../RecordingsView';
import {
  deleteRecording,
  editTranscript,
  getHitWaveform,
  getHits,
  getRecordingsSummary,
  transcribeHit,
} from '../../../../api/recordings';
import type { HitDetail, RecordingsSummary } from '../../../../types';
import { useStore } from '../../../../store/useStore';

vi.mock('../../../../api/recordings', () => ({
  getHits: vi.fn(),
  getRecordingsSummary: vi.fn(),
  getHitWaveform: vi.fn(),
  hitAudioUrl: (id: string) => `/api/v1/hits/${id}/audio`,
  transcribeHit: vi.fn(),
  editTranscript: vi.fn(),
  deleteRecording: vi.fn(),
}));

vi.mock('../HitDetailSheet', () => ({
  HitDetailSheet: () => null,
  formatDuration: (seconds?: number | null) =>
    seconds === undefined || seconds === null ? '—' : `${seconds.toFixed(1)}s`,
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
  recording_duration: 3.5,
  recording_bytes: 1024,
  transcript_status: 'completed',
  transcript: 'NS 24 clear at Phoenix',
  ...overrides,
});

const summary = (overrides: Partial<RecordingsSummary> = {}): RecordingsSummary => ({
  count: 1,
  bytes: 1024,
  oldest_timestamp: 1_700_000_000,
  failed_transcripts: 0,
  untranscribed: 0,
  hits_total: 1,
  retention_days: 30,
  ...overrides,
});

describe('RecordingsView', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit()],
      total: 1,
      limit: 50,
      offset: 0,
    });
    vi.mocked(getRecordingsSummary).mockResolvedValue(summary());
    vi.mocked(getHitWaveform).mockResolvedValue({
      buckets: [10, 40, 90, 30],
      peak_dbfs: -3.2,
      clipped: false,
    });
    vi.mocked(transcribeHit).mockResolvedValue({ transcript_status: 'pending' });
    vi.mocked(editTranscript).mockResolvedValue(hit({ transcript: 'Corrected text' }));
    vi.mocked(deleteRecording).mockResolvedValue({
      hit_uid: 'h-1',
      deleted_audio: true,
      transcript_cleared: true,
    });
    vi.spyOn(window.HTMLMediaElement.prototype, 'play').mockResolvedValue(undefined);
    vi.spyOn(window.HTMLMediaElement.prototype, 'pause').mockImplementation(() => {});
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('shows the library totals and filter counts', async () => {
    vi.mocked(getRecordingsSummary).mockResolvedValue(
      summary({
        count: 48,
        bytes: 9_937_152,
        failed_transcripts: 41,
        untranscribed: 20,
        hits_total: 51,
      }),
    );
    render(<RecordingsView />);

    expect(await screen.findByText('48 files')).toBeInTheDocument();
    expect(screen.getByText('9.5 MB')).toBeInTheDocument();
    expect(screen.getByText('keeping 30 days')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^All\s*51$/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^With audio\s*48$/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^Untranscribed\s*20$/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^Failed\s*41$/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /retry 41 failed/i })).toBeInTheDocument();
  });

  it('groups rows by day and totals each group', async () => {
    vi.mocked(getHits).mockResolvedValue({
      hits: [
        hit({ id: '2', hit_uid: 'h-2', timestamp: 1_700_090_000, alpha_tag: 'SEPTA NORTH' }),
        hit(),
      ],
      total: 2,
      limit: 50,
      offset: 0,
    });
    render(<RecordingsView />);

    await screen.findByText('SEPTA NORTH');
    const dayOf = (ts: number) =>
      new Date(ts * 1000).toLocaleDateString(undefined, {
        weekday: 'short',
        month: 'short',
        day: 'numeric',
      });
    expect(screen.getByText(dayOf(1_700_090_000))).toBeInTheDocument();
    expect(screen.getByText(dayOf(1_700_000_000))).toBeInTheDocument();
    expect(screen.getAllByText('1 hit · 3.5s')).toHaveLength(2);
  });

  it('regroups by channel on request', async () => {
    const user = userEvent.setup();
    vi.mocked(getHits).mockResolvedValue({
      hits: [hit({ id: '2', hit_uid: 'h-2', channel: 56, alpha_tag: 'SEPTA NORTH' }), hit()],
      total: 2,
      limit: 50,
      offset: 0,
    });
    render(<RecordingsView />);
    await screen.findByText('SEPTA NORTH');

    await user.selectOptions(screen.getByLabelText('Group'), 'channel');
    expect(screen.getByText('CH 56 · SEPTA NORTH')).toBeInTheDocument();
    expect(screen.getByText('CH 12 · NORFOLK SOUTHERN')).toBeInTheDocument();
  });

  it('asks for the oldest page when the sort flips', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.selectOptions(screen.getByLabelText('Sort'), 'oldest');
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(expect.objectContaining({ order: 'asc' })),
    );
  });

  it('applies the quick filters through the same query', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /^With audio/ }));
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(
        expect.objectContaining({ has_recording: true, transcript_status: undefined }),
      ),
    );

    await user.click(screen.getByRole('button', { name: /^Failed/ }));
    await waitFor(() =>
      expect(getHits).toHaveBeenLastCalledWith(
        expect.objectContaining({ transcript_status: 'failed' }),
      ),
    );
  });

  it('plays in place and reveals the player strip', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /play recording at 160.7250 mhz/i }));

    expect(screen.getByTestId('recordings-audio')).toHaveAttribute('src', '/api/v1/hits/h-1/audio');
    expect(
      screen.getByRole('button', { name: /pause recording at 160.7250 mhz/i }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText('Seek within the recording')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /retry transcript/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^correct$/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^delete$/i })).toBeInTheDocument();
  });

  it('seeks the audio element from the range control', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');
    await user.click(screen.getByRole('button', { name: /play recording/i }));

    const range = screen.getByLabelText('Seek within the recording');
    fireEvent.change(range, { target: { value: '2.5' } });
    expect((screen.getByTestId('recordings-audio') as HTMLAudioElement).currentTime).toBe(2.5);
  });

  it('draws the measured waveform', async () => {
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');

    await waitFor(() => expect(getHitWaveform).toHaveBeenCalledWith('h-1', 32));
  });

  it('flags a clipped recording in the player strip', async () => {
    vi.mocked(getHitWaveform).mockResolvedValue({
      buckets: [100, 100, 100],
      peak_dbfs: -0.1,
      clipped: true,
    });
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /play recording/i }));
    expect(await screen.findByText(/clipped/i)).toBeInTheDocument();
  });

  it('queues a retranscription for the row', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');
    await user.click(screen.getByRole('button', { name: /play recording/i }));

    await user.click(screen.getByRole('button', { name: /retry transcript/i }));
    await waitFor(() => expect(transcribeHit).toHaveBeenCalledWith('h-1', true));
    // The row's status dot becomes the queued state, even though the previous
    // transcript text is still shown as a preview.
    expect(await screen.findByLabelText('Transcription queued')).toBeInTheDocument();
  });

  it('saves an inline transcript correction', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');
    await user.click(screen.getByRole('button', { name: /play recording/i }));

    await user.click(screen.getByRole('button', { name: /^correct$/i }));
    const editor = screen.getByLabelText('Transcript');
    await user.clear(editor);
    await user.type(editor, 'Corrected text');
    await user.click(screen.getByRole('button', { name: /^save$/i }));

    await waitFor(() => expect(editTranscript).toHaveBeenCalledWith('h-1', 'Corrected text'));
    expect(await screen.findByText('Corrected text')).toBeInTheDocument();
  });

  it('requires a second click before deleting the audio', async () => {
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('NORFOLK SOUTHERN');
    await user.click(screen.getByRole('button', { name: /play recording/i }));

    await user.click(screen.getByRole('button', { name: /^delete$/i }));
    expect(deleteRecording).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: /delete for good/i }));
    await waitFor(() => expect(deleteRecording).toHaveBeenCalledWith('h-1'));
  });

  it('loads the next page without losing the rows already shown', async () => {
    vi.mocked(getHits).mockImplementation(async (query = {}) => {
      if (query.offset === 0) {
        return { hits: [hit({ alpha_tag: 'FIRST PAGE' })], total: 2, limit: 50, offset: 0 };
      }
      return {
        hits: [hit({ id: '2', hit_uid: 'h-2', alpha_tag: 'SECOND PAGE' })],
        total: 2,
        limit: 50,
        offset: 50,
      };
    });
    const user = userEvent.setup();
    render(<RecordingsView />);
    await screen.findByText('FIRST PAGE');

    await user.click(screen.getByRole('button', { name: /load older recordings/i }));
    expect(await screen.findByText('SECOND PAGE')).toBeInTheDocument();
    expect(screen.getByText('FIRST PAGE')).toBeInTheDocument();
    expect(screen.getByText('2 of 2 loaded')).toBeInTheDocument();
  });

  it('moves the selection with J and K and plays the selected row with space', async () => {
    vi.mocked(getHits).mockResolvedValue({
      hits: [
        hit({ id: '1', hit_uid: 'h-1', alpha_tag: 'FIRST' }),
        hit({ id: '2', hit_uid: 'h-2', alpha_tag: 'SECOND' }),
      ],
      total: 2,
      limit: 50,
      offset: 0,
    });
    render(<RecordingsView />);
    await screen.findByText('FIRST');

    const list = screen.getByRole('list', { name: 'Recordings' });
    fireEvent.keyDown(list, { key: 'j' });
    expect(screen.getByRole('button', { name: /open hit .*, SECOND/i })).toHaveAttribute(
      'aria-current',
      'true',
    );

    fireEvent.keyDown(list, { key: ' ' });
    expect(screen.getByTestId('recordings-audio')).toHaveAttribute('src', '/api/v1/hits/h-2/audio');
  });

  it('opens the hit sheet when a row is clicked', async () => {
    const onOpenHit = vi.fn();
    const user = userEvent.setup();
    render(<RecordingsView onOpenHit={onOpenHit} />);
    await screen.findByText('NORFOLK SOUTHERN');

    await user.click(screen.getByRole('button', { name: /open hit at 160.7250 mhz/i }));
    expect(onOpenHit).toHaveBeenCalledWith('h-1');
  });

  it('says so when nothing matches', async () => {
    vi.mocked(getHits).mockResolvedValue({ hits: [], total: 0, limit: 50, offset: 0 });
    render(<RecordingsView />);
    expect(await screen.findByText('No recordings match these filters.')).toBeInTheDocument();
  });

  describe('when the user is not recording', () => {
    beforeEach(() => {
      useStore.setState((state) => ({
        preferences: { ...state.preferences, audioRecordingEnabled: false },
      }));
      vi.mocked(getRecordingsSummary).mockResolvedValue(
        summary({ count: 0, bytes: 0, oldest_timestamp: null, hits_total: 93 }),
      );
      vi.mocked(getHits).mockResolvedValue({
        hits: [
          hit({
            recording_status: 'none',
            recording_duration: null,
            recording_bytes: null,
            transcript_status: 'none',
            transcript: null,
          }),
        ],
        total: 1,
        limit: 50,
        offset: 0,
      });
    });

    it('explains that recording is off instead of showing a bare empty library', async () => {
      render(<RecordingsView />);

      expect(await screen.findByText('Hit recording is off')).toBeInTheDocument();
      expect(
        screen.getByText(/Your 93 hits have no audio\. The hits themselves are listed below\./),
      ).toBeInTheDocument();
      expect(screen.getByRole('button', { name: /turn on recording/i })).toBeInTheDocument();
    });

    it('still lists the hits, marked as having no recording', async () => {
      render(<RecordingsView />);

      expect(await screen.findByText('NORFOLK SOUTHERN')).toBeInTheDocument();
      expect(screen.getByLabelText('No recording')).toBeInTheDocument();
      expect(screen.queryByLabelText('Not transcribed')).not.toBeInTheDocument();
    });

    it('drops the audio-only filters and the shortcuts that need audio', async () => {
      render(<RecordingsView />);
      await screen.findByText('Hit recording is off');

      expect(screen.queryByRole('button', { name: /^With audio/ })).not.toBeInTheDocument();
      expect(screen.queryByRole('button', { name: /^Failed/ })).not.toBeInTheDocument();
      expect(screen.queryByText('transcribe')).not.toBeInTheDocument();
      expect(screen.queryByText('delete')).not.toBeInTheDocument();
      // Moving between rows still works.
      expect(screen.getByText('move')).toBeInTheDocument();
    });

    it('does not fire a transcription for a hit with no audio', async () => {
      render(<RecordingsView />);
      await screen.findByText('NORFOLK SOUTHERN');

      fireEvent.keyDown(screen.getByRole('list', { name: 'Recordings' }), { key: 't' });
      expect(transcribeHit).not.toHaveBeenCalled();
    });

    it('switches to the waiting message once recording is enabled', async () => {
      useStore.setState((state) => ({
        preferences: { ...state.preferences, audioRecordingEnabled: true },
      }));
      render(<RecordingsView />);

      expect(await screen.findByText('Recording is on — nothing caught yet')).toBeInTheDocument();
      expect(screen.getByRole('button', { name: /recording settings/i })).toBeInTheDocument();
    });
  });
});
