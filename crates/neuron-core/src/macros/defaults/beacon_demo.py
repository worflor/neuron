"""A tiny BEACON demo — asks one yes/no WITH context, then acts (mocked when you Test it)."""
def macro(ctx):
    import neuron
    if neuron.ask("ship the build?", description="pushes 12 commits to origin/main · CI is green"):
        neuron.type_text("shipping it!")
        return "shipped"
    return "held"
