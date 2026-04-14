---
title: Concepts
description: The vocabulary portagenty uses, defined precisely.
---

The full architectural deep-dive lives in
[DESIGN.md](https://github.com/cybersader/portagenty/blob/main/DESIGN.md)
in the repo. This page is the short version.

## Project

A directory on disk with code or content you work on. Registered with
portagenty at any of three tiers (global, workspace, per-project). A
project is identified by its filesystem path.

## Workspace

A named, curated view over one or more projects plus the sessions you
use to work on them. A first-class file on disk, designed to be
committable. A workspace is where "hierarchy on top of hierarchy"
happens — you can have a `recent` view, a tag-filtered view, a
custom-ordered group view over the same underlying projects.

## Session

One unit of execution: a shell, a process, an agent. Defined by *name +
cwd + command*. A session belongs to a workspace.

## Multiplexer

tmux, zellij, or WezTerm. The thing that actually owns terminal panes
and keeps them alive across detaches. portagenty drives it; it does not
replace it.

## Adapter

Code inside portagenty that speaks to one specific multiplexer. v1 ships
a tmux adapter; zellij and WezTerm adapters follow in v1.x.

## Hierarchy on top of hierarchy

The defining idea. A filesystem forces every project into exactly one
parent directory. portagenty layers *alternative hierarchies* over that
fixed hierarchy without moving anything on disk. Same project, many
views — by recency, by tag, by custom group.
