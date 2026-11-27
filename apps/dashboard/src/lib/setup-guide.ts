// The setup guide's ticks, derived from what the workspace really has. Nothing here is
// a flag the page sets: a step is done because the thing exists.

export const DEFAULT_WORKSPACE_NAME = 'Personal workspace';

export type GuideFacts = {
	workspaceName: string | undefined;
	monitorCount: number;
	webhookUrl: string | null;
	/** Checks committed for the first monitor. */
	observed: number;
};

export type GuideSteps = {
	named: boolean;
	watching: boolean;
	alerting: boolean;
	observed: boolean;
	/** Naming is a courtesy; a target, an alert channel and first evidence are the setup. */
	complete: boolean;
};

export function guideSteps(facts: GuideFacts): GuideSteps {
	const named = (facts.workspaceName ?? DEFAULT_WORKSPACE_NAME) !== DEFAULT_WORKSPACE_NAME;
	const watching = facts.monitorCount > 0;
	const alerting = facts.webhookUrl !== null;
	const observed = facts.observed > 0;
	return { named, watching, alerting, observed, complete: watching && alerting && observed };
}
