import type { DisplayState, OwnerState, UiState } from './projection';
import { readDisplayState, readSelection } from './reading-state';
import { DRAFT_KEY, displayStorage, initialSections, initialTheme, storageGet } from './ui-preferences';
import { initialActionReceipts, storedActions, storedSubmissions } from './pending-inputs';

export function initialState(): UiState {
  const storage = displayStorage();
  const persisted = readDisplayState(storage);
  const actions = storedActions();
  const selection = readSelection(storage);
  const display: DisplayState = {
    view: 'timeline',
    panel_open: false,
    open_sections: initialSections(persisted),
    theme: initialTheme(),
    reading: persisted.reading,
    new_count: 0,
    scroll_request: 0,
    wrap: persisted.wrap,
    ...(persisted.anchor ? { anchor: persisted.anchor } : {}),
    ...(selection ? { selection } : {}),
  };
  return {
    source: 'live',
    connection: 'connecting',
    reconnect_seconds: 0,
    capabilities: {
      append: false,
      steer: false,
      withdraw: false,
      actions: false,
      documents: false,
      attachments: false,
      durable_input_receipts: false,
      committed_replay: false,
      streaming: false,
    },
    append_available: false,
    durable_receipts: false,
    committed_replay: false,
    streaming: false,
    documents: [],
    document_errors: [],
    action_receipts: initialActionReceipts(actions),
    subject_id: null,
    wanted_identity_revision: null,
    step: null,
    timeline: [],
    submissions: storedSubmissions(),
    draft: storageGet(DRAFT_KEY),
    attachments: [],
    config_error: false,
    config_entries: undefined,
    config_entries_error: false,
    config_drafts: {},
    self_error: false,
    tasks_error: false,
    memory_error: false,
    playground_error: false,
  /** The ownership facts. */
  owner_state: undefined as OwnerState | undefined,
  /** The name the reader is trying to give the instance. */
  name_draft: '',
  /** The claim capability the reader pasted, if any. */
  claim_draft: '',
    display,
  };
}
