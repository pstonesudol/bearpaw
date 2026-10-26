import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HitDetailSheet } from '../HitDetailSheet';
import { getHit, deleteRecording, editTranscript, transcribeHit } from '../../../../api/recordings';
import type { HitDetail } from '../../../../types';

vi.mock('../../../../api/recordings', () => ({
  getHit: vi.fn(),
  editTranscript: vi.fn(),
  transcribeHit: vi.fn(),
  deleteRecording: vi.fn(),
  hitAudioUrl: (id: string) => `http://localhost:8000/api/v1/hits/${id}/audio`,
}));

vi.mock('sonner', () => ({
  toast: { success: vi.fn(), error: vi.fn() },
}));

vi.mock('../../../../store/useStore', () => {
  const updateActivityLogEntry = vi.fn();
  const useStore = (selector: (state: unknown) => unknown) => selector({ updateActivityLogEntry });
  useStore.getState = () => ({ updateActivityLogEntry });
  return { useStore };
});

const baseHit: HitDetail = {
  id: '42',
  timestamp: 1_700_000_000,
  frequency: 161.1,
  channel: 12,
  alpha_tag: 'AMTRAK',
  bank: 3,
  duration: 4.2,
  ended_at: 1_700_000_004,
  scanner_id: 'BC125AT',
  hit_uid: 'h-1',
  recording_status: 'ready',
  recording_file: 'h-1.wav',
  recording_duration: 4.9,
  recording_bytes: 156_800,
  link_confidence: 'exact',
  transcript_status: 'completed',
  transcript: 'Amtrak 215 clear at Zoo',
  transcript_engine: 'whisper-cli',
};

describe('HitDetailSheet', () => {
  const onClose = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getHit).mockResolvedValue(baseHit);
    vi.mocked(editTranscript).mockResolvedValue({
      ...baseHit,
      transcript: 'Amtrak 215 clear at Zoo interlocking',
      transcript_original: 'Amtrak 215 clear at Zoo',
    });
    vi.mocked(transcribeHit).mockResolvedValue({ transcript_status: 'pending' });
    vi.mocked(deleteRecording).mockResolvedValue({
      hit_uid: 'h-1',
      deleted_audio: true,
      transcript_cleared: true,
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('shows the hit, a playable recording and its transcript', async () => {
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);

    expect(await screen.findByText('AMTRAK')).toBeInTheDocument();
    expect(screen.getByText('161.1000 MHz')).toBeInTheDocument();
    expect(screen.getByText('Amtrak 215 clear at Zoo')).toBeInTheDocument();
    const audio = document.querySelector('audio');
    expect(audio).not.toBeNull();
    expect(audio).toHaveAttribute('src', 'http://localhost:8000/api/v1/hits/h-1/audio');
    expect(screen.getByText(/4.9s of audio/)).toBeInTheDocument();
  });

  it('keeps the machine transcript visible after a correction', async () => {
    vi.mocked(getHit).mockResolvedValue({
      ...baseHit,
      transcript_original: 'Amtrak 2 1 5 clear at Zoo',
    });
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);
    await screen.findByText('Amtrak 215 clear at Zoo');

    expect(await screen.findByText('Machine transcript')).toBeInTheDocument();
    expect(screen.getByText('Amtrak 2 1 5 clear at Zoo')).toBeInTheDocument();
  });

  it('saves an edit without claiming the machine output was replaced', async () => {
    const user = userEvent.setup();
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);
    await screen.findByText('Amtrak 215 clear at Zoo');

    await user.click(screen.getByRole('button', { name: /correct transcript/i }));
    const box = screen.getByLabelText('Transcript text');
    await user.clear(box);
    await user.type(box, 'Amtrak 215 clear at Zoo interlocking');
    await user.click(screen.getByRole('button', { name: /save correction/i }));

    await waitFor(() =>
      expect(editTranscript).toHaveBeenCalledWith('h-1', 'Amtrak 215 clear at Zoo interlocking'),
    );
    expect(await screen.findByText('Amtrak 215 clear at Zoo interlocking')).toBeInTheDocument();
  });

  it('offers a retry that queues the hit again', async () => {
    const user = userEvent.setup();
    vi.mocked(getHit).mockResolvedValue({
      ...baseHit,
      transcript_status: 'failed',
      transcript: null,
      transcript_error: 'transcription_empty: whisper-cli produced no text',
    });
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);
    await screen.findByText(/whisper-cli produced no text/);

    await user.click(screen.getByRole('button', { name: /retry transcription/i }));
    await waitFor(() => expect(transcribeHit).toHaveBeenCalledWith('h-1', false));
    expect(await screen.findByText('Queued')).toBeInTheDocument();
  });

  it('warns when the file and the hit are only probably linked', async () => {
    vi.mocked(getHit).mockResolvedValue({
      ...baseHit,
      recording_status: 'partial',
      link_confidence: 'uncertain',
    });
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);
    expect(await screen.findByText(/may not be the whole transmission/i)).toBeInTheDocument();
  });

  it('deletes only after a second confirmation', async () => {
    const user = userEvent.setup();
    vi.mocked(deleteRecording).mockResolvedValue({
      hit_uid: 'h-1',
      deleted_audio: true,
      transcript_cleared: true,
    });
    vi.mocked(getHit)
      .mockResolvedValueOnce(baseHit)
      .mockResolvedValueOnce({
        ...baseHit,
        recording_status: 'deleted',
        recording_file: null,
        transcript: null,
        transcript_status: 'none',
      });
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);
    await screen.findByText('Amtrak 215 clear at Zoo');

    await user.click(screen.getByRole('button', { name: /delete recording/i }));
    expect(deleteRecording).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: /^delete$/i }));

    await waitFor(() => expect(deleteRecording).toHaveBeenCalledWith('h-1'));
    expect(await screen.findByText('Recording deleted')).toBeInTheDocument();
  });

  it('explains why a too-short transmission has no audio', async () => {
    vi.mocked(getHit).mockResolvedValue({
      ...baseHit,
      recording_status: 'too_short',
      recording_file: null,
      recording_duration: 0.2,
      transcript_status: 'none',
      transcript: null,
    });
    render(<HitDetailSheet hitUid="h-1" onClose={onClose} />);
    expect(await screen.findByText(/shorter than the configured minimum/i)).toBeInTheDocument();
    expect(document.querySelector('audio')).toBeNull();
  });
});
