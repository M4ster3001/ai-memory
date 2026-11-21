# Agent bus between projects (under construction)

Nothing on this page is built yet. Today mail to another project is delivered
to its inbox but does not wake its agent, unless an operator lists the sending
project in a configured supervisor entry's `wake_from`
([`docs/agent-messaging.md`](../agent-messaging.md#who-may-wake-an-agent)).

The design, from the owner's decisions of November 2026:

1. You ask, in a prompt in project A, for your agent to talk to the agent of
   project X.
2. The server checks that your prompt, as the hooks captured it, named X, and
   files a connection request. A request never wakes a stopped agent.
3. The agent of X shows the request to the user of X in its own chat and asks.
   Only that user's answer, captured by X's hooks after the request was shown,
   opens the connection.
4. While the connection is open, mail between A and X wakes like mail inside
   one project. It closes when either user closes it or when it expires, and
   every request, answer and wake is audited.

Until then, a person can still relay between projects: ask the agent in A to
write what it needs, and paste it into a prompt in X.
