# Glyphtrail

Glyphtrail turns source repositories into a queryable code graph so that coding
agents and people can see a codebase's structure, its API boundaries, and the
blast radius of a change, within one repo and across many.

## Graph

**Code Graph**:
The structural model of one repo: nodes for code elements and edges for the
relationships between them.
_Avoid_: knowledge graph (marketing only), database, model

**Index**:
A repo's persisted Code Graph plus the metadata needed to query and refresh it.
Each repo owns exactly one Index; there is no central one.
_Avoid_: DB, store, cache

**Node**:
One element of a Code Graph: a repo, directory, file, Symbol, Comment, or API
element.
_Avoid_: vertex, entity

**Symbol**:
A named code element that can be defined and referred to: a function, method,
type, trait, module, or constant.
_Avoid_: identifier, declaration, item

**Edge**:
A directed, typed relationship between two Nodes, such as a call, an import, or
containment.
_Avoid_: relation, link (reserved for Cross-Repo Link)

**Confidence**:
How an Edge or match was established: **Extracted** when read directly from
the source, **Inferred** when resolved heuristically.
_Avoid_: certainty, score

**Design Rationale**:
A comment that explains why code is the way it is, attached to the Symbol it
documents.
_Avoid_: doc comment, annotation

**Freshness**:
Whether an Index still reflects its repo: **fresh**, **stale** (the code or the
analyzer changed since the last analysis), or **unknown**.
_Avoid_: up-to-date, dirty

**Sensitive File**:
A file whose content must never enter an Index, such as a key or credentials
file.
_Avoid_: secret file, private file

## Impact

**Seed**:
The thing that changed, as the starting point of an impact analysis: a Symbol,
a file, or a set of changes.
_Avoid_: target, root, origin

**Blast Radius**:
Every Node that a change to the Seeds could affect, each with its distance from
a Seed and the weakest Confidence on the path that reached it.
_Avoid_: impact set, affected set, fallout

**Risk Level**:
A coarse rating of a Blast Radius: LOW, MEDIUM, HIGH or CRITICAL.
_Avoid_: severity, score

**Gate**:
A check that fails a CI run when a change touches the API Surface or causes
Drift.
_Avoid_: guard, policy

## API boundary

**Endpoint**:
A server-side API operation that a handler serves: a REST route, gRPC method,
GraphQL field, or WebSocket channel.
_Avoid_: route, handler (the handler is the Symbol that serves the Endpoint)

**Client Call**:
A place in code that calls a remote API operation.
_Avoid_: call site, request, consumer call

**Router**:
A composable group of Endpoints that can be mounted under a path prefix.
_Avoid_: blueprint, sub-app

**API Surface**:
The set of Endpoints a repo exposes.
_Avoid_: public API (ambiguous with Package Exports)

**Schema**:
An API contract declared outside the code (OpenAPI, protobuf, GraphQL SDL,
Hasura metadata) that the team treats as the source of truth.
_Avoid_: spec, contract file, blessed schema

**Schema Operation**:
One operation declared in a Schema.
_Avoid_: schema endpoint

**Operation Key**:
The protocol-independent identity of an API operation: protocol, method, and
normalized path. Endpoints, Client Calls and Schema Operations are compared by
it.
_Avoid_: signature, route key

**Path Rewrite**:
A mapping between the path a client uses and the path the server sees, such as
a gateway stripping `/api`.
_Avoid_: alias, redirect

**Drift**:
A mismatch between the Endpoints in code and the Schema Operations in a
Schema.
_Avoid_: divergence, schema diff

## Many repos

**Registry**:
The user-wide list of known repos by name and location. It holds no graph
data.
_Avoid_: catalog, workspace

**Group**:
A named subset of the Registry that narrows a Federated Query.
_Avoid_: workspace, project, collection

**Forge Identity**:
A stable id for a repo derived from its git remotes, so one repo is recognised
across clones, renames and mirrors.
_Avoid_: repo id, fingerprint

**Package Identity**:
What a repo publishes (packages and their exports) and which external packages
it uses. It is what lets repos be linked by dependency.
_Avoid_: manifest, metadata

**Producer**:
A repo whose package or Endpoint another repo depends on.
_Avoid_: upstream, provider, server repo

**Consumer**:
A repo that depends on a Producer's package or calls a Producer's Endpoint.
_Avoid_: downstream repo, dependent, client repo

**Cross-Repo Link**:
A connection from a Consumer's use of a package to the Producer that publishes
it, at symbol level where possible and whole-package level otherwise.
_Avoid_: edge, dependency edge

**Link Hint**:
A Cross-Repo Link declared by hand, for relationships no package dependency
expresses, such as an HTTP call between services.
_Avoid_: manual link, override

**Federated Query**:
A query that spans several repos' Indexes at once, joined through Cross-Repo
Links at query time.
_Avoid_: global query, cross-repo search

**Downstream**:
Toward Consumers. Downstream impact is the part of a Blast Radius that lies in
other repos.
_Avoid_: dependents

## Atlas

**Atlas**:
An opt-in, private, local-only history index across all registered repos:
commits, authors and topics rather than code structure.
_Avoid_: archive, history DB

**Author**:
A raw name-and-email pair as it appears on a commit.
_Avoid_: committer, user

**Identity**:
The single person behind one or more Authors.
_Avoid_: user, profile, account

**Topic**:
A theme derived from commits that spans commits and repos.
_Avoid_: tag, label, cluster

**Digest**:
A structured summary of one repo (languages, dependencies, API) used to
compare repos.
_Avoid_: profile, summary, fingerprint

## Agent onboarding

**Managed Section**:
The block that onboarding writes into a repo's agent instructions file and
owns; text outside it is never touched.
_Avoid_: snippet, injected block
