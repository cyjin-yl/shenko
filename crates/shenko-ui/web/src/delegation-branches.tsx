import { text as t } from './messages';
import * as React from 'react';
import type { TaskSummary } from './projection';
import { isDelegation, trajectoryMessages, childParticipant, type Participant, type DemoThreadData, type ThreadMessage, type TrajectoryResponse } from './threads';
import { Icon } from './icons';
import { CollapsibleRoot, CollapsibleTrigger, CollapsibleContent } from './ui-kit';

/// What Inspect needs beyond the projection: the task list (to find the
/// delegations a round started) and a way to open a thread in 对话.
export const InspectContext = React.createContext<{
  tasks: TaskSummary[];
  openThread: (id: string) => void;
  subject: Participant;
  demo?: DemoThreadData;
}>({ tasks: [], openThread: () => undefined, subject: { id: 'subject', name: t('subject.name'), role: 'subject' } });

/// The delegations one committed round started, branching off the spine:
/// each is its own trajectory, readable inline or as a group thread in 对话.
export function DelegationBranches({ stepId }: { stepId: string }) {
  const { tasks, openThread, subject, demo } = React.useContext(InspectContext);
  const delegations = tasks.filter((task) => isDelegation(task) && task.created_by_step_id === stepId);
  if (delegations.length === 0) return null;
  return <div className="branches">{delegations.map((task) => (
    <DelegationBranch key={String(task.task_id)} task={task} subject={subject} demo={demo}
      onOpen={() => openThread(`delegation:${String(task.task_id)}`)} />
  ))}</div>;
}

function DelegationBranch({ task, subject, demo, onOpen }: {
  task: TaskSummary; subject: Participant; demo?: DemoThreadData; onOpen: () => void;
}) {
  const id = String(task.task_id);
  const child = childParticipant(id);
  const [open, setOpen] = React.useState(false);
  const [trace, setTrace] = React.useState<ThreadMessage[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  React.useEffect(() => {
    if (!open || demo) return;
    let live = true;
    fetch(`/api/tasks/${encodeURIComponent(id)}/trajectory`, { cache: 'no-store' })
      .then((response) => { if (!response.ok) throw new Error(String(response.status)); return response.json(); })
      .then((data: TrajectoryResponse) => { if (live) setTrace(trajectoryMessages(data, subject)); })
      .catch((reason) => { if (live) setError(String(reason?.message ?? reason)); });
    return () => { live = false; };
  }, [open, id, String(task.state), demo]);
  return (
    <CollapsibleRoot className="branch" open={open} onOpenChange={(detail: { open: boolean }) => setOpen(detail.open)}>
      <div className="branch-head">
        <CollapsibleTrigger className="branch-trigger">
          <Icon name="chevron" size={14} />
          <span className="branch-title">{t('delegation.heading', { name: child.name })}</span>
          <span className="branch-state" data-state={String(task.state)}>{String(task.state)}</span>
        </CollapsibleTrigger>
        <button type="button" className="text-action" onClick={onOpen}><Icon name="chat" size={14} />{t('delegation.openChat')}</button>
      </div>
      <CollapsibleContent className="branch-body">
        {demo ? <ol className="trace">{trajectoryMessages(demo.trajectory(id), subject).map((item) => (
          <li key={item.key} className={`trace-row role-${item.from.role} kind-${item.kind}`}>
            <span className="trace-who">{item.kind === 'system' ? '·' : item.from.name}</span>
            <span className="trace-text">{item.tag ? <b>{item.tag} </b> : null}{item.text || t('common.noText')}</span>
            {item.meta && <span className="trace-meta">{item.meta}</span>}
          </li>
        ))}</ol>
          : error ? <p className="muted is-error">{t('delegation.readError', { detail: error })}</p>
            : !trace ? <p className="muted">{t('delegation.loading')}</p>
              : <ol className="trace">{trace.map((item) => (
                <li key={item.key} className={`trace-row role-${item.from.role} kind-${item.kind}`}>
                  <span className="trace-who">{item.kind === 'system' ? '·' : item.from.name}</span>
                  <span className="trace-text">{item.tag ? <b>{item.tag} </b> : null}{item.text || t('common.noText')}</span>
                  {item.meta && <span className="trace-meta">{item.meta}</span>}
                </li>
              ))}</ol>}
      </CollapsibleContent>
    </CollapsibleRoot>
  );
}
