# A Systems View of Recursive Self Improvement

RSI (recursive self improvement) is a commonly used phrase in AI. It's
generally used to describe using AI to speed up the process of making AI, even
if just applying it to a narrow function, such as creating GPU kernels or data
cleaning.

This is an overly broad definition of recursive, and is better described as
autocatalytic or using a technology to speed up the development of that
technology. Computers, the steam engine, and the Internet have all been
autocatalytic.

Recursive, on the other hand, suggests that you use a complete version of a
thing to create another complete version of that thing (similar to function
recursion in programming languages). Using a language's compiler to build the
next version of that compiler is arguably recursive. The distinction matters
because recursion, unlike autocatalysis, requires the system to carry its own
state forward and to do so safely. It's the strong form of a system being able to
build versions of itself with minimal outside involvement.

The bitter lesson argues that general methods ultimately beat hand-engineered
ones. Applied to agents, the hand-engineered harness is the next thing to fall,
as models get smarter, they should have largely unfettered ability to modify
their own harnesses rather than living inside one we froze for them. The
question is: how do you provide the agent maximum flexibility to evolve itself,
while providing minimal scaffolding to do it safely?

Exo is an attempt to answer that question. It has direct access to its running
code, so it can modify nearly every aspect of itself. Further, it has basic
support to clone, rewind, rebuild, and re-run itself.[^1]

However, just having full access to read and modify code isn't quite sufficient
for a long running, working system. Even with recursion in languages there is
runtime support, in particular language scoping and the call stack. Without
that, a recursive function could not maintain the state it needs to perform
arbitrary compute, and it would be tremendously difficult to do safely.

Within Exo we similarly provide runtime support for the agent to recursively
improve itself. The canonical state provides an immutable, append-only log of
all events, not exactly a call stack but more a complete execution history that
nothing can erase. It maintains full lineage across clones. And even when the
sandbox is rewound, the log is preserved. An agent that breaks itself, rewinds,
and tries again can see what it already tried, instead of repeating the same
mistake in a loop. The exo-harness provides the minimal infrastructure to
protect secrets and the core mechanism for managing this state. It is the only
part of Exo which cannot be modified by the agent.[^2]

That's the whole design. Exo supports recursive improvement by allowing the
model to modify any aspect of itself, clone itself, rebuild and restart itself,
and rewind itself, and to do all this incrementally and somewhat safely, with
minimal scaffolding.

[^1]:
    To be clear, we're focused on recursive improvement of the agent and not
    the agent and the underlying model. Arguably a fully RSI solution would also
    improve the model, the compute underneath it, and the power feeding it all.
    Perhaps an RSI harness is a step on that path.

[^2]:
    Whether or not the agent can modify the exo-harness is actually a policy
    consideration. The system technically allows it, but to provide safer standard
    usage, it's disallowed on the default configuration.
