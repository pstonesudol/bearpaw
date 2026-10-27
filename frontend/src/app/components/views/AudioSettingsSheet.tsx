import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Activity,
  AudioLines,
  FolderOpen,
  Loader2,
  Mic,
  Play,
  RotateCcw,
  TriangleAlert,
} from 'lucide-react';
import { toast } from 'sonner';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../ui/select';
import { Slider } from '../ui/slider';
import { Switch } from '../ui/switch';
import { cn } from '../../../lib/utils';
import { useStore, type Preferences } from '../../../store/useStore';
import { API_BASE } from '../../../api/useApi';
import { getAudioDevices, getAudioStatus, testAudioRecording } from '../../../api/recordings';
import { isTauriRuntime, pickDirectory } from '../../../tauri-shell';
import type { AudioDeviceInfo, RecordingStatusSnapshot, WhisperModelInfo } from '../../../types';
import { formatDuration } from './HitDetailSheet';

export interface AudioSettingsSheetProps {
  isOpen: boolean;
  onClose: () => void;
}

/** The backend's default vocabulary, shown as a placeholder and restorable. */
export const DEFAULT_VOCABULARY =
  'SEPTA, Norfolk Southern, Amtrak, NORAC, Harrisburg Line, Keystone Corridor, ' +
  'Zoo Interlocking, Phoenix, Paoli, Thorndale, Norristown, Powelton Yard, ' +
  'clear, approach, restricted speed, dispatcher, maintenance of way';

function levelPercent(db: number): number {
  if (!Number.isFinite(db)) return 0;
  return Math.max(0, Math.min(100, Math.round(((db + 90) / 90) * 100)));
}

function RangeRow({
  id,
  label,
  help,
  value,
  min,
  max,
  step,
  suffix,
  onChange,
}: {
  id: string;
  label: string;
  help: string;
  value: number;
  min: number;
  max: number;
  step: number;
  suffix: string;
  onChange: (value: number) => void;
}) {
  // Local while dragging so the number follows the thumb; the PUT goes out on
  // commit. DeviceTab saves on every change, which is fine for a switch but
  // means a slider drag writes a preference dozens of times.
  //
  // `null` means "show the stored value". Clearing it on commit is what lets a
  // save round-trip back without an effect syncing state from a prop.
  const [draft, setDraft] = useState<number | null>(null);
  const shown = draft ?? value;
  return (
    <div className="flex items-center justify-between gap-6">
      <div className="space-y-0.5">
        <label htmlFor={id} className="text-sm font-medium text-white">
          {label}
        </label>
        <p className="text-xs text-white/55">{help}</p>
      </div>
      <div className="flex shrink-0 items-center gap-3">
        <Slider
          id={id}
          aria-label={label}
          value={[shown]}
          onValueChange={(values) => setDraft(values[0])}
          onValueCommit={(values) => {
            onChange(values[0]);
            setDraft(null);
          }}
          min={min}
          max={max}
          step={step}
          className="w-[var(--size-select-wide)]"
        />
        <span className="w-16 text-right font-mono text-sm text-white/70">
          {shown}
          {suffix}
        </span>
      </div>
    </div>
  );
}

export function AudioSettingsSheet({ isOpen, onClose }: AudioSettingsSheetProps) {
  const preferences = useStore((state) => state.preferences);
  const updatePreferences = useStore((state) => state.updatePreferences);
  const [devices, setDevices] = useState<AudioDeviceInfo[]>([]);
  const [models, setModels] = useState<WhisperModelInfo[]>([]);
  const [modelsDir, setModelsDir] = useState('');
  const [status, setStatus] = useState<RecordingStatusSnapshot | null>(null);
  const [resolvedBinary, setResolvedBinary] = useState<string | null>(null);
  const [resolvedModel, setResolvedModel] = useState<string | null>(null);
  const [loadingDevices, setLoadingDevices] = useState(false);
  const [testing, setTesting] = useState(false);
  // `null` means "follow the saved preference". Typing sets a draft; a
  // successful save clears it. No effect needs to copy props into state.
  const [binaryDraft, setBinaryDraft] = useState<string | null>(null);
  const [vocabularyDraft, setVocabularyDraft] = useState<string | null>(null);
  const mounted = useRef(true);
  const statusPollBusy = useRef(false);

  const save = useCallback(
    async <K extends keyof Preferences>(key: K, backendKey: string, value: Preferences[K]) => {
      try {
        const response = await fetch(`${API_BASE}/preferences`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ [backendKey]: value }),
        });
        if (!response.ok) throw new Error(`Preference save failed (${response.status})`);
        updatePreferences({ [key]: value } as Partial<Preferences>);
      } catch (error) {
        console.error('Failed to save audio preference', error);
        toast.error('Failed to save that setting. Your previous value is still active.');
      }
    },
    [updatePreferences],
  );

  const loadDevices = useCallback(async () => {
    setLoadingDevices(true);
    try {
      const response = await getAudioDevices();
      if (!mounted.current) return;
      setDevices(response.devices);
      setModels(response.models);
      setModelsDir(response.models_dir);
      setStatus(response.status);
      setResolvedBinary(response.transcription.resolved_binary ?? null);
      setResolvedModel(response.transcription.resolved_model ?? null);
    } catch (error) {
      console.warn('Could not list audio devices', error);
    } finally {
      if (mounted.current) setLoadingDevices(false);
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    // Data-fetch lifecycle: `loadDevices` flips `loadingDevices` before awaiting
    // the backend. That is not derived state and there is no render-time
    // equivalent for "fetch when the dialog opens".
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void loadDevices();
  }, [isOpen, loadDevices]);

  // The level meter only has a value while capture is running. Poll it while
  // the sheet is open and stop the moment it closes.
  useEffect(() => {
    if (!isOpen || !preferences.audioRecordingEnabled) return;
    const timer = window.setInterval(async () => {
      // One request at a time: a slow status read must not queue up behind
      // itself for the whole time the sheet is open.
      if (statusPollBusy.current) return;
      statusPollBusy.current = true;
      try {
        const next = await getAudioStatus();
        if (mounted.current) setStatus(next);
      } catch {
        // A background poll that fails must not toast; the sheet already shows
        // the last known state.
      } finally {
        statusPollBusy.current = false;
      }
    }, 500);
    return () => window.clearInterval(timer);
  }, [isOpen, preferences.audioRecordingEnabled]);

  const handleTest = async () => {
    setTesting(true);
    try {
      const clip = await testAudioRecording(3);
      toast.success(`Test recording captured ${formatDuration(clip.duration_secs)} of audio`);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : 'Test recording failed');
    } finally {
      setTesting(false);
    }
  };

  const handlePickFolder = async () => {
    if (!isTauriRuntime()) {
      toast.error('Changing the recordings folder is only available in the desktop app.');
      return;
    }
    const directory = await pickDirectory();
    if (!directory) return;
    await save('audioStorageDir', 'audio_storage_dir', directory);
    toast.success('Recordings folder updated. Existing files are not moved.');
  };

  const capture = status?.capture;
  const captureError = capture?.last_error;
  const level = levelPercent(capture?.level_db ?? -90);
  const hasAudio = devices.length > 0 || !!preferences.audioInputDevice;

  const storageLabel = useMemo(() => {
    if (preferences.audioStorageDir.trim()) return preferences.audioStorageDir.trim();
    return `${status?.recordings_dir ?? '…'} (default)`;
  }, [preferences.audioStorageDir, status?.recordings_dir]);

  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex max-h-[88vh] max-w-3xl flex-col overflow-hidden border-white/10 bg-[#12151b] text-white">
        <DialogTitle className="shrink-0 pr-8 text-xl font-bold">
          Audio &amp; Transcription
        </DialogTitle>
        <p className="shrink-0 text-sm text-white/55">
          Record each scanner hit from a line-in adapter and transcribe it locally. Nothing is
          uploaded.
        </p>
        {/* The close button is absolutely positioned inside DialogContent, so
            the content itself must not scroll: a sheet this tall scrolled the X
            off the top. Only the body scrolls. */}
        <div className="min-h-0 flex-1 space-y-5 overflow-y-auto pr-1">
          <section className="space-y-4" aria-label="Recording">
            <h3 className="flex items-center gap-2 text-sm font-bold uppercase tracking-wider text-white/70">
              <Mic aria-hidden className="size-4" /> Recording
            </h3>

            <div className="space-y-3 rounded-lg border border-white/10 bg-black/20 p-4">
              <div className="flex items-center justify-between gap-6">
                <div className="space-y-0.5">
                  <label
                    htmlFor="audio-recording-enabled"
                    className="text-sm font-medium text-white"
                  >
                    Record scanner hits
                  </label>
                  <p className="text-xs text-white/55">
                    Capture audio continuously while monitoring and keep one file per transmission.
                  </p>
                </div>
                <Switch
                  id="audio-recording-enabled"
                  checked={preferences.audioRecordingEnabled}
                  onCheckedChange={(checked) =>
                    void save('audioRecordingEnabled', 'audio_recording_enabled', checked)
                  }
                />
              </div>

              <div className="flex items-center justify-between gap-6 border-t border-white/5 pt-3">
                <div className="space-y-0.5">
                  <label className="text-sm font-medium text-white">Input device</label>
                  <p className="text-xs text-white/55">
                    The Cubilux adapter appears here as a USB audio input.
                  </p>
                </div>
                <div className="w-[var(--size-select-wide)] shrink-0">
                  <Select
                    value={preferences.audioInputDevice || '__default__'}
                    onValueChange={(value) =>
                      void save(
                        'audioInputDevice',
                        'audio_input_device',
                        value === '__default__' ? '' : value,
                      )
                    }
                  >
                    <SelectTrigger aria-label="Audio input device" className="scanner-input">
                      <SelectValue placeholder={loadingDevices ? 'Looking…' : 'System default'} />
                    </SelectTrigger>
                    <SelectContent className="scanner-select-content">
                      <SelectItem value="__default__">System default</SelectItem>
                      {devices.map((device) => (
                        <SelectItem key={device.name} value={device.name}>
                          {device.name}
                          {device.is_default ? ' (default)' : ''}
                        </SelectItem>
                      ))}
                      {preferences.audioInputDevice &&
                        !devices.some((device) => device.name === preferences.audioInputDevice) && (
                          <SelectItem value={preferences.audioInputDevice}>
                            {preferences.audioInputDevice} (not connected)
                          </SelectItem>
                        )}
                    </SelectContent>
                  </Select>
                </div>
              </div>

              <div className="space-y-1 border-t border-white/5 pt-3">
                <div className="flex items-center justify-between text-xs text-white/55">
                  <span className="flex items-center gap-1.5">
                    <Activity aria-hidden className="size-3.5" /> Input level
                  </span>
                  <span className="font-mono">
                    {capture?.connected ? `${Math.round(capture.level_db)} dBFS` : '—'}
                  </span>
                </div>
                <div
                  className="h-2 overflow-hidden rounded-full bg-white/10"
                  role="meter"
                  aria-label="Input level"
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-valuenow={level}
                >
                  <div
                    className={cn(
                      'h-full rounded-full transition-[width] duration-150',
                      level > 90 ? 'bg-red-400' : level > 60 ? 'bg-amber-400' : 'bg-emerald-400',
                    )}
                    style={{ width: `${level}%` }}
                  />
                </div>
                <div className="flex items-center justify-between pt-1">
                  <p className="text-xs text-white/45">
                    {preferences.audioRecordingEnabled
                      ? capture?.connected
                        ? `Capturing from ${capture.device ?? 'the selected input'}`
                        : (captureError ?? 'Waiting for the input device…')
                      : 'Enable recording to see a live level.'}
                  </p>
                  <button
                    type="button"
                    onClick={() => void handleTest()}
                    disabled={testing || !hasAudio}
                    className="inline-flex shrink-0 items-center gap-1.5 rounded border border-white/10 bg-white/5 px-2.5 py-1 text-xs hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    {testing ? (
                      <Loader2 aria-hidden className="size-3.5 animate-spin" />
                    ) : (
                      <Play aria-hidden className="size-3.5" />
                    )}
                    Test recording
                  </button>
                </div>
                {captureError && (
                  <p className="flex items-start gap-1.5 pt-1 text-xs text-amber-200">
                    <TriangleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
                    {captureError}
                  </p>
                )}
              </div>
            </div>

            <div className="space-y-4 rounded-lg border border-white/10 bg-black/20 p-4">
              <RangeRow
                id="audio-pre-roll"
                label="Pre-roll"
                help="Audio kept from just before the squelch opened."
                value={preferences.audioPreRollMs}
                min={0}
                max={2000}
                step={50}
                suffix=" ms"
                onChange={(value) => void save('audioPreRollMs', 'audio_pre_roll_ms', value)}
              />
              <RangeRow
                id="audio-post-roll"
                label="Post-roll"
                help="Audio kept after the squelch closed."
                value={preferences.audioPostRollMs}
                min={0}
                max={3000}
                step={50}
                suffix=" ms"
                onChange={(value) => void save('audioPostRollMs', 'audio_post_roll_ms', value)}
              />
              <RangeRow
                id="audio-min-duration"
                label="Minimum recording"
                help="Shorter transmissions are discarded as noise."
                value={preferences.audioMinDurationMs}
                min={100}
                max={5000}
                step={100}
                suffix=" ms"
                onChange={(value) =>
                  void save('audioMinDurationMs', 'audio_min_duration_ms', value)
                }
              />
            </div>

            <div className="space-y-3 rounded-lg border border-white/10 bg-black/20 p-4">
              <div className="flex items-center justify-between gap-6">
                <div className="min-w-0 space-y-0.5">
                  <span className="text-sm font-medium text-white">Recordings folder</span>
                  <p className="break-all text-xs text-white/55">{storageLabel}</p>
                </div>
                <button
                  type="button"
                  onClick={() => void handlePickFolder()}
                  className="inline-flex shrink-0 items-center gap-1.5 rounded border border-white/10 bg-white/5 px-2.5 py-1 text-xs hover:bg-white/10"
                >
                  <FolderOpen aria-hidden className="size-3.5" /> Change
                </button>
              </div>
              <RangeRow
                id="audio-retention"
                label="Keep audio for"
                help="0 days keeps audio until the hit itself ages out of history."
                value={preferences.audioRetentionDays}
                min={0}
                max={365}
                step={1}
                suffix=" d"
                onChange={(value) => void save('audioRetentionDays', 'audio_retention_days', value)}
              />
            </div>
          </section>

          <section className="space-y-4" aria-label="Transcription">
            <h3 className="flex items-center gap-2 text-sm font-bold uppercase tracking-wider text-white/70">
              <AudioLines aria-hidden className="size-4" /> Transcription
            </h3>

            <div className="space-y-3 rounded-lg border border-white/10 bg-black/20 p-4">
              <div className="flex items-center justify-between gap-6">
                <div className="space-y-0.5">
                  <label htmlFor="transcription-enabled" className="text-sm font-medium text-white">
                    Transcribe new recordings
                  </label>
                  <p className="text-xs text-white/55">
                    Runs offline with whisper.cpp. Audio never leaves this Mac.
                  </p>
                </div>
                <Switch
                  id="transcription-enabled"
                  checked={preferences.transcriptionEnabled}
                  onCheckedChange={(checked) =>
                    void save('transcriptionEnabled', 'transcription_enabled', checked)
                  }
                />
              </div>

              <div className="flex items-center justify-between gap-6 border-t border-white/5 pt-3">
                <div className="space-y-0.5">
                  <label className="text-sm font-medium text-white">Model</label>
                  <p className="text-xs text-white/55">
                    GGML models from{' '}
                    <span className="font-mono">{modelsDir || 'the models folder'}</span>.
                  </p>
                </div>
                <div className="w-[var(--size-select-wide)] shrink-0">
                  <Select
                    value={preferences.transcriptionModel || '__auto__'}
                    onValueChange={(value) =>
                      void save(
                        'transcriptionModel',
                        'transcription_model',
                        value === '__auto__' ? '' : value,
                      )
                    }
                  >
                    <SelectTrigger aria-label="Transcription model" className="scanner-input">
                      <SelectValue placeholder="Auto-detect" />
                    </SelectTrigger>
                    <SelectContent className="scanner-select-content">
                      <SelectItem value="__auto__">Auto-detect</SelectItem>
                      {models.map((model) => (
                        <SelectItem key={model.name} value={model.name}>
                          {model.name}
                        </SelectItem>
                      ))}
                      {preferences.transcriptionModel &&
                        !models.some((model) => model.name === preferences.transcriptionModel) && (
                          <SelectItem value={preferences.transcriptionModel}>
                            {preferences.transcriptionModel} (not found)
                          </SelectItem>
                        )}
                    </SelectContent>
                  </Select>
                </div>
              </div>

              {preferences.transcriptionEnabled && !resolvedBinary && (
                <p className="flex items-start gap-1.5 rounded border border-amber-400/20 bg-amber-500/10 p-2 text-xs leading-relaxed text-amber-100">
                  <TriangleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
                  whisper-cli was not found. Install it with <code>brew install whisper-cpp</code>,
                  or set its path below. Recording works without it.
                </p>
              )}
              {preferences.transcriptionEnabled && !resolvedModel && (
                <p className="flex items-start gap-1.5 rounded border border-amber-400/20 bg-amber-500/10 p-2 text-xs leading-relaxed text-amber-100">
                  <TriangleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
                  No GGML model found. Put a <code>.bin</code> model in{' '}
                  {modelsDir || 'the models folder'}.
                </p>
              )}

              <div className="space-y-1 border-t border-white/5 pt-3">
                <label htmlFor="transcription-binary" className="text-sm font-medium text-white">
                  whisper-cli path
                </label>
                <p className="text-xs text-white/55">
                  Leave empty to search PATH and Homebrew's usual locations.
                </p>
                <div className="flex gap-2 pt-1">
                  <input
                    id="transcription-binary"
                    aria-label="whisper-cli path"
                    value={binaryDraft ?? preferences.transcriptionBinary}
                    onChange={(event) => setBinaryDraft(event.target.value)}
                    onBlur={() => {
                      const next = (binaryDraft ?? preferences.transcriptionBinary).trim();
                      setBinaryDraft(null);
                      if (next !== preferences.transcriptionBinary) {
                        void save('transcriptionBinary', 'transcription_binary', next).then(() =>
                          loadDevices(),
                        );
                      }
                    }}
                    placeholder={resolvedBinary ?? 'whisper-cli'}
                    className="w-full rounded border border-white/15 bg-black/30 px-2 py-1.5 font-mono text-sm text-white placeholder:text-white/30"
                  />
                </div>
              </div>

              <div className="space-y-1 border-t border-white/5 pt-3">
                <div className="flex items-center justify-between">
                  <label
                    htmlFor="transcription-vocabulary"
                    className="text-sm font-medium text-white"
                  >
                    Vocabulary
                  </label>
                  <button
                    type="button"
                    onClick={() => {
                      setVocabularyDraft(null);
                      void save('transcriptionVocabulary', 'transcription_vocabulary', '');
                    }}
                    className="inline-flex items-center gap-1 text-xs text-white/50 hover:text-white"
                  >
                    <RotateCcw aria-hidden className="size-3" /> Use default
                  </button>
                </div>
                <p className="text-xs text-white/55">
                  Terms whisper should spell correctly. Comma-separated; long lists are truncated.
                </p>
                <textarea
                  id="transcription-vocabulary"
                  aria-label="Transcription vocabulary"
                  value={vocabularyDraft ?? preferences.transcriptionVocabulary}
                  rows={3}
                  onChange={(event) => setVocabularyDraft(event.target.value)}
                  onBlur={() => {
                    if (vocabularyDraft === null) return;
                    const next = vocabularyDraft;
                    setVocabularyDraft(null);
                    if (next !== preferences.transcriptionVocabulary) {
                      void save('transcriptionVocabulary', 'transcription_vocabulary', next);
                    }
                  }}
                  placeholder={DEFAULT_VOCABULARY}
                  className="w-full rounded border border-white/15 bg-black/30 p-2 text-sm text-white placeholder:text-white/30"
                />
              </div>

              <p className="text-xs text-white/40">
                Transcripts are best-effort. Radio noise, clipping and local proper nouns all cause
                errors; check the audio before relying on a transcript.
              </p>
            </div>
          </section>
        </div>
      </DialogContent>
    </Dialog>
  );
}
