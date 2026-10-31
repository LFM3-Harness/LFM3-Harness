import { useState } from 'react';

import { ChatView } from './chat/ChatView';
import WorkspaceView from './WorkspaceView';

type View = 'chat' | 'tasks';

export default function App() {
  // Demo fixtures describe verified runs only; chat is unavailable there.
  const demo =
    import.meta.env.DEV && new URLSearchParams(window.location.search).get('demo') === '1';
  const [view, setView] = useState<View>(demo ? 'tasks' : 'chat');

  if (view === 'tasks') {
    return (
      <WorkspaceView onOpenChat={demo ? undefined : () => setView('chat')} />
    );
  }
  return <ChatView onOpenTasks={() => setView('tasks')} />;
}
