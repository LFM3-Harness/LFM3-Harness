/**
 * The workspace (folder) switcher. It sits at the top of the sidebar, where
 * assistants usually put project and workspace pickers, and appears as a
 * compact pill in the mobile chat header.
 */
export function WorkspaceSwitcher({
  label,
  title,
  disabled,
  busy,
  compact = false,
  onChange,
}: {
  label: string;
  title: string;
  disabled: boolean;
  busy: boolean;
  compact?: boolean;
  onChange: () => void;
}) {
  return (
    <button
      type="button"
      className={`workspace-switcher ${compact ? 'workspace-switcher-compact' : ''}`}
      disabled={disabled}
      title={title}
      onClick={onChange}
    >
      <span className="workspace-switcher-icon" aria-hidden="true">
        ▸
      </span>
      <span className="workspace-switcher-text">
        {compact ? null : <span className="workspace-switcher-label">Folder</span>}
        <span className="workspace-switcher-name">{label}</span>
      </span>
      <span className="workspace-switcher-change">{busy ? 'Choosing…' : 'Change'}</span>
    </button>
  );
}
