import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ScanView } from '../ScanView';
import { useStore } from '../../../../store/useStore';
import { createTestActivityLogEntry, createTestLiveState } from '../../../../test/fixtures';

// The detail sheet loads the durable row as soon as it opens. Mocking the media
// API keeps this test about the Recent Hits row that opened it.
vi.mock('../../../../api/recordings', () => ({
  getHit: vi.fn().mockResolvedValue({
    id: '42',
    timestamp: 100,
    frequency: 160.725,
    channel: 12,
    alpha_tag: 'NORFOLK SOUTHERN',
    duration: 3.5,
    hit_uid: 'h-1',
    recording_status: 'ready',
    recording_duration: 4,
    transcript_status: 'completed',
    transcript: 'NS 24 clear at Phoenix',
  }),
  editTranscript: vi.fn(),
  transcribeHit: vi.fn(),
  deleteRecording: vi.fn(),
  hitAudioUrl: (id: string) => `/api/v1/hits/${id}/audio`,
}));

vi.mock('sonner', () => ({
  toast: { success: vi.fn(), error: vi.fn() },
}));

const baseProps = {
  mainText: 'Scanning...',
  subText: '',
  scannerMode: 'SCAN' as const,
  connectionStatus: 'connected' as const,
  isHolding: false,
  isInitialSyncing: false,
  chartAnimate: false,
  dashboardLoading: false,
  busiestChannels: [],
  hourlyHeatmap: [],
  heatmapStats: { max: 0 },
  squelch: 0,
  onSquelchChange: () => {},
  onHoldToggle: () => {},
  onLockout: () => {},
  onVolumeChange: () => {},
  onBankToggle: () => {},
  onOpenActivityExport: () => {},
};

describe('ScanView recordings', () => {
  beforeEach(() => {
    useStore.setState({
      liveState: createTestLiveState({ mode: 'SCAN', squelch_open: false }),
      banks: Array(10).fill(true),
      fullActivityLog: [],
    });
  });

  it('marks a hit with audio and shows a transcript preview', () => {
    useStore.setState({
      fullActivityLog: [
        createTestActivityLogEntry({
          id: 'h-1',
          timestamp: 100,
          frequency: 160.725,
          channel: 12,
          alpha_tag: 'NORFOLK SOUTHERN',
          hit_uid: 'h-1',
          recording_status: 'ready',
          transcript_status: 'completed',
          transcript: 'NS 24 clear at Phoenix',
        }),
      ],
    });
    render(<ScanView {...baseProps} />);

    expect(screen.getByLabelText('Has a recording')).toBeInTheDocument();
    expect(screen.getByLabelText('Transcribed')).toBeInTheDocument();
    expect(screen.getByText('NS 24 clear at Phoenix')).toBeInTheDocument();
  });

  it('does not claim audio for a hit whose recording failed', () => {
    useStore.setState({
      fullActivityLog: [
        createTestActivityLogEntry({
          id: 'h-2',
          timestamp: 100,
          channel: 12,
          recording_status: 'failed',
          transcript_status: 'none',
        }),
      ],
    });
    render(<ScanView {...baseProps} />);

    expect(screen.queryByLabelText('Has a recording')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Transcribed')).not.toBeInTheDocument();
  });

  it('opens the durable hit when a Recent Hits row is selected', async () => {
    const user = userEvent.setup();
    useStore.setState({
      fullActivityLog: [
        createTestActivityLogEntry({
          id: 'h-1',
          timestamp: 100,
          frequency: 160.725,
          channel: 12,
          alpha_tag: 'NORFOLK SOUTHERN',
          hit_uid: 'h-1',
          recording_status: 'ready',
          transcript_status: 'completed',
          transcript: 'NS 24 clear at Phoenix',
        }),
      ],
    });
    render(<ScanView {...baseProps} />);

    await user.click(screen.getByRole('button', { name: /open hit at 160\.725 mhz/i }));
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toBeInTheDocument();
    const audio = dialog.querySelector('audio');
    expect(audio).toHaveAttribute('src', '/api/v1/hits/h-1/audio');
  });
});
