"""A benchmark harness for version control tools: one spec file describes an experiment, and the
harness prepares two hosts, shapes the link between them, runs the matrix and records the results.

See ../README.md. The package runs in three places: on the operator's machine (the `bench` command),
on the client host (the agent, which runs the matrix), and on both hosts (host operations the agent
calls).
"""

# The measurement method. Bump it whenever a change alters what the harness measures or how, so that
# results taken before and after never land in the same series.
# 2: a profile's round trip is the one the tools see; the shaper adds only what the unshaped link
#    lacks of it. Before, it added the whole round trip on top of the unshaped one.
METHOD = 2
