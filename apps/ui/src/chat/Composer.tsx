import { useEffect, useRef, useState } from 'react';

import { canRecord, encodeVoiceClip, startMicrophone } from './audio';

const MAX_SECONDS = 120;

export function Composer({
  disabled,
  running,
  rows = 3,
  placeholder = 'Ask anything, or describe what you’d like done…',
  onSend,
  onStop,
  onTranscribe,
  onAttachImage,
  onError,
}: {
  disabled: boolean;
  running: boolean;
  rows?: number;
  placeholder?: string;
  onSend: (text: string) => void;
  onStop: () => void;
  onTranscribe: (base64: string, format: string) => Promise<string>;
  onAttachImage: (file: File) => Promise<string>;
  onError: (message: string) => void;
}) {
  const [text, setText] = useState('');
  const [recording, setRecording] = useState(false);
  const [transcribing, setTranscribing] = useState(false);
  const [attaching, setAttaching] = useState(false);
  const [seconds, setSeconds] = useState(0);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const chunksRef = useRef<Blob[]>([]);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const fileRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    return () => {
      recorderRef.current?.stop();
      streamRef.current?.getTracks().forEach((track) => track.stop());
    };
  }, []);

  useEffect(() => {
    if (!recording) {
      return;
    }
    const timer = window.setInterval(() => setSeconds((value) => value + 1), 1000);
    return () => window.clearInterval(timer);
  }, [recording]);

  useEffect(() => {
    if (recording && seconds >= MAX_SECONDS) {
      recorderRef.current?.stop();
    }
  }, [recording, seconds]);

  function submit() {
    const value = text.trim();
    if (value.length === 0 || disabled || running) {
      return;
    }
    onSend(value);
    setText('');
    textareaRef.current?.focus();
  }

  async function toggleRecording() {
    if (recording) {
      recorderRef.current?.stop();
      return;
    }
    try {
      const stream = await startMicrophone();
      streamRef.current = stream;
      const recorder = new MediaRecorder(stream);
      chunksRef.current = [];
      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) {
          chunksRef.current.push(event.data);
        }
      };
      recorder.onstop = () => {
        stream.getTracks().forEach((track) => track.stop());
        streamRef.current = null;
        recorderRef.current = null;
        setRecording(false);
        setSeconds(0);
        void finishRecording(recorder.mimeType, chunksRef.current);
      };
      recorder.start();
      recorderRef.current = recorder;
      setSeconds(0);
      setRecording(true);
    } catch (cause) {
      onError(`Microphone unavailable: ${messageOf(cause)}`);
    }
  }

  async function finishRecording(mimeType: string, chunks: Blob[]) {
    if (chunks.length === 0) {
      return;
    }
    setTranscribing(true);
    try {
      const blob = new Blob(chunks, { type: mimeType || 'audio/webm' });
      const clip = await encodeVoiceClip(blob);
      const transcript = await onTranscribe(clip.base64, 'wav');
      setText((current) => (current.trim() ? `${current.trim()} ${transcript}` : transcript));
      textareaRef.current?.focus();
    } catch (cause) {
      onError(`Voice input failed: ${messageOf(cause)}`);
    } finally {
      setTranscribing(false);
    }
  }

  const micAvailable = canRecord();

  async function attach(file: File | undefined) {
    if (!file) {
      return;
    }
    setAttaching(true);
    try {
      const path = await onAttachImage(file);
      setText((current) => {
        const instruction = `Analyse the attached image ${path} and describe what it shows.`;
        return current.trim().length > 0 ? `${current.trim()}\n\n${instruction}` : instruction;
      });
      textareaRef.current?.focus();
    } catch (cause) {
      onError(`Image attachment failed: ${messageOf(cause)}`);
    } finally {
      setAttaching(false);
      if (fileRef.current) {
        fileRef.current.value = '';
      }
    }
  }

  return (
    <section className="composer" aria-label="Message composer">
      <textarea
        ref={textareaRef}
        value={text}
        rows={rows}
        placeholder={placeholder}
        disabled={disabled || transcribing || attaching}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (
            event.key === 'Enter' &&
            !event.shiftKey &&
            !event.nativeEvent.isComposing
          ) {
            event.preventDefault();
            submit();
          }
        }}
      />
      <div className="composer-bar">
        <span className="composer-hint">
          {recording
            ? `Recording ${formatSeconds(seconds)} — click the mic to stop`
            : transcribing
              ? 'Transcribing locally…'
              : attaching
                ? 'Attaching the image to the workspace…'
                : 'Enter to send · Shift+Enter for a new line'}
        </span>
        <div className="composer-actions">
          <input
            ref={fileRef}
            type="file"
            accept="image/png,image/jpeg,image/webp,image/gif"
            className="visually-hidden"
            onChange={(event) => void attach(event.target.files?.[0])}
          />
          <button
            type="button"
            className="icon-button attach-button"
            aria-label="Add an image"
            title="Add an image"
            disabled={running || transcribing || attaching}
            onClick={() => fileRef.current?.click()}
          >
            <span className="attach-plus" aria-hidden="true">
              ＋
            </span>
            <span className="attach-label">Image</span>
          </button>
          {recording ? <span className="record-dot" aria-hidden="true" /> : null}
          {recording ? (
            <button
              type="button"
              className="icon-button recording stop-mic-button"
              aria-label="Stop recording"
              title="Stop recording"
              onClick={() => void toggleRecording()}
            >
              <span className="record-square" aria-hidden="true" />
            </button>
          ) : running ? (
            <button type="button" className="danger stop-button" onClick={onStop}>
              <span className="stop-label">Stop</span>
              <span className="stop-square" aria-hidden="true" />
            </button>
          ) : text.trim().length > 0 ? (
            <button
              type="button"
              className="primary send-button"
              aria-label="Send"
              disabled={disabled || transcribing || attaching}
              onClick={submit}
            >
              <span className="send-label">Send</span>
              <span className="send-arrow" aria-hidden="true">
                ↑
              </span>
            </button>
          ) : micAvailable ? (
            <button
              type="button"
              className="icon-button mic-button"
              aria-label="Record a voice message"
              title="Record a voice message"
              disabled={transcribing}
              onClick={() => void toggleRecording()}
            >
              <MicIcon />
            </button>
          ) : (
            <button
              type="button"
              className="primary send-button"
              aria-label="Send"
              disabled
              onClick={submit}
            >
              <span className="send-label">Send</span>
              <span className="send-arrow" aria-hidden="true">
                ↑
              </span>
            </button>
          )}
        </div>
      </div>
    </section>
  );
}

function MicIcon() {
  return (
    <svg viewBox="0 0 20 20" width="18" height="18" fill="none" aria-hidden="true">
      <rect x="7" y="2.4" width="6" height="9.2" rx="3" fill="currentColor" />
      <path
        d="M4.4 9.4a5.6 5.6 0 0 0 11.2 0M10 15v2.4M7.5 17.4h5"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
      />
    </svg>
  );
}

function formatSeconds(value: number): string {
  const minutes = Math.floor(value / 60);
  const seconds = value % 60;
  return `${minutes}:${seconds.toString().padStart(2, '0')}`;
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
