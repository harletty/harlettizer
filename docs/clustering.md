# Clustering

[`hz-cluster`](../crates/hz-cluster) folds an authored object scene into the
fixed number of elements a delivery bitstream carries. A programme is mixed
with as many objects as it needs, often more than a hundred; a bitstream
carries twelve, fourteen or sixteen. Something has to decide which objects
share an element, where it goes, and how its audio is made.

This is the one part of the project with no published answer, and the part a
listener hears. So the first thing built was not an algorithm but **the number
that judges one**.

## What an element count means, and what it meant before

An element count in this notebook is what the **bitstream carries**, which is
what `harlettizer encode --cluster` means and what `cargo xtask cluster
--elements` means since September 2026. The low frequency channel is one of
them: it takes an element and is not clustered — it is not a direction, and
nothing here has any say about where it goes — so a master with one folds into
`N - 1` clustered elements.

**Every table written before then names one fewer element than the stream it
describes.** The harness counted the clustered elements only, so it folded a
master one element wider than the encoder did, and on a contested fold one
element is the whole difference: on a real programme at what both called
eleven, the encoder's fold left an element of what a listener notices moving
where the harness's fold left none. The tables are not re-measured and are not
wrong about what they compared; a row headed "twelve elements" is a fold into
twelve clustered elements beside the low frequency channel, which this now
calls thirteen.

Two things were wrong at once and both are fixed. The other was the geometry's
timing: the harness placed a block with the state in force at the block's
**start**, and the encoder reads a span and holds where every object was at its
**end**. On a master whose first statements land a few samples in, that folded
the first block with every object at the default centre, and the warm start
carried a different numbering through the rest of the programme. With both
settled the two agree element for element in 88 % of blocks and to within the
three decimals a report prints everywhere else, and the ruler of
[`hz_cluster::motion`](../crates/hz-cluster/src/motion.rs) reports the same
figures from either side.

## What makes an answer right

Rendering is linear in the object signals. On a layout with `C` speakers, an
object at `p` with signal `x` contributes `g(p)·x`. If each element `k` sits at
`q_k` and carries `y_k = Σ_i w_ik x_i`, the rendered result is

```text
Σ_k g(q_k)·y_k  =  Σ_i (Σ_k w_ik g(q_k))·x_i     against     Σ_i g(p_i)·x_i
```

So the whole difference is **per object**, it depends on the weights and
positions alone, and it can be measured **without any audio**: how far
`Σ_k w_ik g(q_k)` is from `g(p_i)`, as a fraction of `g(p_i)`, weighted by how
loud that object is. An object nobody can hear landing in the wrong place is not
a defect anybody can hear either.

That is what `hz_cluster::metric::error` computes, and everything below is
judged by it. It runs over a whole scene from the metadata and the audio's
levels, in the time it takes to pan a few thousand positions — which is what
makes it possible to *measure* a change instead of arguing about it.

**The anchor**: with as many elements as objects, the fold must cost nothing.
It does — `5.9e-8` at its worst for the panned weighting and exactly zero for
the nearest one. Without that check every other number here would be
unfalsifiable.

## Where the weights come from

Not from a rule invented here. Given the element positions, distributing an
object over them **by panning it onto them** preserves its energy and its
energy-weighted direction — a property of vector-base amplitude panning, using
the same geometry `hz-render` already pans objects onto speakers with, over
arbitrary points instead of a named layout.

That leaves only where to put the elements: a clustering of the objects **by
direction**, weighted by loudness. Two objects the same way round render almost
identically however far apart they are, so distance is not what an element has
to agree about.

**Which panner the 7.1.4 layer is.** The numbers in this document were taken
while that layer was an external VBAP panner; since 2026-09-09 it is the one in
[`hz-render`](../crates/hz-render/src/panner.rs), written from the published
method. On the forty-object scene of [encode.md](encode.md), on the same code,
the swap moves the mean fold cost from 0.0517 to 0.0527 at twelve elements,
0.0354 to 0.0359 at fourteen and 0.0257 to 0.0256 at sixteen, and leaves the
worst object unchanged — under two per cent, inside the spread between two
runs of the search. The tables below stand as measured.

## Three weightings, and how to tell which is right

An object that does not sit on an element has to be dealt with somehow, and
there are three answers. Two of them answer a question about the object's
**direction**; the third answers the question the object actually poses, which
is about the shape of what it radiates.

Folding a 48-object scene into twelve elements, over **every presentation the
stream is played through** — 2.0, 5.1 and 7.1 as the reference *folds* them,
and 7.1.4 as a renderer pans it:

| | Mean error | Worst object | Level |
|---|---|---|---|
| Spread — pan it onto the elements | 0.219 | 0.809 | **1.20** |
| Nearest — give it to one | 0.154 | 0.765 | 1.0022 |
| **Fitted — solve for what it radiates** | **0.058** | **0.370** | **1.0041** |

Fitting is better on every axis by a factor of three, and it is the default.
Per presentation, which is what a single-layout measurement could not show:

| Elements | | 2.0 | 5.1 | 7.1 | 7.1.4 |
|---|---|---|---|---|---|
| 12 | Spread | 0.247 | 0.224 | 0.208 | 0.196 |
| 12 | Nearest | 0.088 | 0.135 | 0.174 | 0.219 |
| 12 | **Fitted** | **0.042** | **0.051** | **0.065** | **0.075** |
| 16 | Spread | 0.239 | 0.221 | 0.210 | 0.205 |
| 16 | Nearest | 0.064 | 0.094 | 0.120 | 0.152 |
| 16 | **Fitted** | **0.028** | **0.032** | **0.040** | **0.051** |

`Nearest` and `Fitted` are *better* the narrower the presentation and `Spread`
is worse, which is the geometry: a fold onto two channels cannot tell two
elements apart that a 7.1.4 can, so giving an object to one element costs less
there — and spreading it over several costs more, because the spill the wider
layout kept apart lands on the same speaker.

### The old table, and what was wrong with it

Measured on 7.1.4 alone, which is what this said before:

| | Mean error | Worst object | Level |
|---|---|---|---|
| Spread | 0.239 | 0.538 | 1.16 – 1.25 |
| Nearest | 0.175 | 0.628 | 1.0000 |
| Fitted | 0.065 | 0.439 | 1.0000 |

Two things it could not see.

**The elements were in the wrong frame.** A master's object positions are a
*cube* and the clustering worked entirely in directions — assignment by angle,
centres as normalised means, a hysteresis in degrees. That is the right domain
for the decision, and it throws away the radius. A renderer never noticed,
because vector-base amplitude panning reads a direction; a fold reads the cube,
and an element left on the unit sphere at `[0.707, 0.707, 0]` is not where its
objects were at `[0.5, 0.5, 0]`. Measured on real masters before the radius
was handed back: the 7.1.4 render put the error at **0.001** and the
three folds put it at **0.38 to 0.49**. It was the frame, not the clustering —
handing the radius back takes all three folds to 0.000.

**The level was preserved against the layout it was fitted to.** `Fitted`
restores an object's level by scaling the weights until what they radiate is as
loud as the object was — over the reference the fit is made against. Made
against 7.1.4's directions, that layout came out at 1.0000 and the folds came
out **5 % loud**. The fit is now made against the presentations themselves,
stacked into one tall vector, so the quantity optimised is the one the metric
reports. That is worth 20 to 30 % of the remaining error as well:

| Elements | Fitted against 7.1.4's directions | Fitted against the presentations |
|---|---|---|
| 6 | 0.202, level 1.055 | **0.174**, level **1.007** |
| 8 | 0.131, level 1.046 | **0.105**, level **1.006** |
| 12 | 0.076, level 1.034 | **0.058**, level **1.004** |
| 16 | 0.054, level 1.023 | **0.038**, level **1.004** |

The level is now held over the presentations rather than on any one of them:
7.1.4 alone comes out at 0.97 to 0.99 and stereo at 1.02 to 1.07. Holding one
layout exactly was never the property worth having.

### How many elements one object may reach

Every element an object is spread over is a path its signal takes to a speaker,
and paths a wide layout keeps apart arrive together on a narrow one. So the
number is worth bounding rather than leaving to the fit. Folding the same
48-object ring, over all four presentations:

| Elements | Cap | Mean | Worst | Level | Paths an object |
|---|---|---|---|---|---|
| 12 | 2 | 0.086 | 0.421 | 1.0045 | 1.79 |
| 12 | **3** | **0.058** | **0.370** | 1.0042 | 2.06 |
| 12 | 4 | 0.058 | 0.370 | 1.0041 | 2.17 |
| 12 | none | 0.058 | 0.370 | 1.0041 | 2.17 |
| 16 | 2 | 0.077 | 0.346 | 1.0052 | 1.69 |
| 16 | **3** | **0.038** | **0.308** | 1.0035 | 1.98 |
| 16 | 4 | 0.038 | 0.308 | 1.0035 | 2.00 |
| 16 | none | 0.038 | 0.308 | 1.0035 | 2.00 |

Three is free and two is not. Left to itself the fit already uses about 2.1
elements an object, so the cap costs a ten-thousandth of the level and nothing
of the error, and it turns "about two" from an observation into a bound. Two
costs half as much again in error, which is what a triangle being the smallest
thing that spans a direction looks like — it is also what vector-base amplitude
panning uses, and for the same reason.

### The worst object, and the floor under it

`worst` is the bound on what a listener could localise wrongly, so it has to
leave out objects nobody localises at all — otherwise it becomes a report about
the quietest thing in the scene, since a quiet object is exactly the one a fold
moves furthest. The floor is **forty decibels under the loudest object in the
block, in amplitude**, which is the unit `Object::energy` carries: a gain times
a signal level, not a power.

### An element carrying nothing is an element going spare

On a real master the worst case was **1.339**, and the object setting it
was at the ceiling corner `[1, 1, 1]`, 38 dB under the loudest, folded into an
element at `[1, 1, 0.0003]` — the corner directly *below* it. Its height was
thrown away completely, which is why the error exceeded one: what it got was
not a displaced version of its own gain vector but a different one.

The scene had eleven objects and twelve elements, and the twelfth was carrying
nothing at all.

The clustering is energy-weighted from end to end — the seeding picks by energy
times how badly served an object is, the centres are energy-weighted means —
which is right for deciding *where* elements go and wrong for deciding whether
to use one. An element carrying nothing costs nothing, and the object it serves
best is whichever is furthest from its own element, weighted by nothing at all.
Handing the spares out that way, the same master reports **zero on every
presentation**, mean and worst alike: every object gets an element of its own,
which is what having enough of them means.

It changes nothing where there is nothing to spare — the tables above are
48 objects into six to sixteen elements — so it is a fix for exactly the case
it was found in, which is also the case a delivery stream is usually in:
shipped masters carry eleven to fifteen objects and the bitstream carries
twelve to sixteen elements.

### An element's position is not its direction

The clustering decides in **directions** — assignment by angle, centres as
normalised means, a hysteresis in degrees — and the position it hands back is
that direction scaled by a radius and clamped into the cube. The clamp *turns*
it: a direction of `[0.673, 0.673, 0.303]` at radius 1.57 scales to
`[1.06, 1.06, 0.48]` and clamps to `[1.00, 1.00, 0.48]`, which points about a
degree away.

So a warm start that recovers the direction by normalising the previous
position does not recover it. The clustering carries the settled directions
alongside the positions instead. The consequence of not doing so is bounded and
rare — the ownership pass runs before the centres are recomputed, so a rotation
of a degree only changes the answer for an object already within a degree of
the five-degree hysteresis, and on real masters it changes nothing measurable
— but re-deriving an answer through a lossy projection is not something to
leave in place because it usually survives.

### Placing an element on the metric — the proxy was the problem

The section below records this being tried and rejected. It is now done, and it
wins at every element count, because what was rejected was a **proxy**.

The quantity the metric reports is

```text
Σ_i  e_i · ‖ Σ_k w_ik·r(q_k) − r(p_i) ‖² / ‖ r(p_i) ‖²
```

and no mean of gain vectors appears anywhere in it. What was measured before
was the position whose gain vector best matches the energy-weighted mean of its
members' — an average taken in one space and inverted through a non-linear map
into another, which is exactly the objection the old section raises against
itself. Minimising the expression directly is a different thing: move one
element, keep the rest, ask what the metric would say, keep the move if it says
less.

A ring of 48 objects, the same scene the table below uses:

| Elements | on directions | on the metric |
|---|---|---|
| 6 | 0.174 / 0.948 | **0.161 / 0.870** |
| 8 | 0.105 / 0.602 | **0.087 / 0.510** |
| 12 | 0.058 / 0.370 | **0.048 / 0.339** |
| 16 | 0.038 / 0.308 | **0.033 / 0.212** |

And on the masters:

| | error mean / worst | travel a block | flips a block |
|---|---|---|---|
| crowd40, 12 elements, before | 0.044 / 0.377 | 2.51° | 4.74 |
| crowd40, 12 elements, after | **0.036 / 0.325** | 2.52° | **2.78** |
| crowd40, 16 elements, before | 0.035 / 0.293 | 2.51° | 5.40 |
| crowd40, 16 elements, after | **0.029 / 0.208** | 2.52° | **3.97** |
| prog11, 12 elements | 0.000 / 0.000 | 0.02° | 0.02 |
| cc, 12 elements | 0.000 / 0.000 | — | 0.00 |

The two masters whose fold is the identity are untouched, as they have to be:
with an element for every object there is nothing to gain and so nothing moves.

#### How it is cheap enough to do

Moving element `k` changes only the objects whose weight on `k` is not zero,
and for each of those it changes one term of a sum. So the sum without `k` is
computed once per element, and each candidate costs one panning of the
candidate position plus a dot product per object it touches. Four passes of six
candidates — a step along each cube axis, both ways, halving each pass — take
the clustering from 58 µs a block to 268 µs, which is 48 s over a two-hour
programme against the encoder's 21.5 minutes: 3.7 % of the encode rather than
0.8 %.

| passes | mean | worst | a block |
|---|---|---|---|
| 0 | 0.044 | 0.377 | 58 µs |
| 2 | 0.042 | 0.306 | 185 µs |
| 3 | 0.039 | 0.318 | 235 µs |
| **4** | **0.036** | **0.325** | **268 µs** |
| 6 | 0.036 | 0.318 | 385 µs |

A long first step is not a wider search but a worse one — it lands the element
in a basin the descent then climbs back out of, and the flip count says so:
a tenth of the cube costs the same as a half and beats it (0.036 against 0.039,
2.78 flips against 5.31).

#### What it cost in stream, and what it costs now

Measured on its own, against the state before any of this landed: **+5.1 %** on
`crowd40` folded to twelve, 2 704 010 B against 2 841 462 B, and +0.0002 % on
`prog11`, whose fold is the identity. A better fold spreads an object over more
elements — 2.65 an object against 2.22 — and an element that is a denser
mixture is a harder signal to code losslessly.

It was not the search dithering: made to demand a fraction of the score before
it moves at all, the error came back and so did the size, in step —

| a move must save | mean error | paths | bytes |
|---|---|---|---|
| nothing | 0.036 | 2.65 | 2 841 462 |
| 3 % | 0.037 | 2.63 | 2 844 580 |
| 10 % | 0.040 | 2.43 | 2 803 688 |
| 30 % | 0.044 | 2.22 | 2 723 892 |

— and it was not the path count either, since capping an object at two elements
makes the stream **larger** (3 093 016 B), for the reason `docs/encode.md`
already gives.

**With the rest of the work in, the cost is gone.** Holding an object's channels
between blocks leaves the elements more stationary, and that is worth more to a
lossless coder than the spread costs it. Same scene, everything merged:

| `--fold-search` | fold, mean / worst | paths | flips a block | bytes |
|---|---|---|---|---|
| 0 | 0.0597 / 0.366 | 2.19 | 2.79 | 2 925 504 |
| 2 | 0.0545 / 0.337 | | | 2 849 516 |
| **4** (default) | **0.0517 / 0.332** | 2.63 | **2.35** | **2 847 202** |
| 6 | 0.0519 / 0.328 | | | 2 883 196 |

A seventh off what the fold costs **and** a fortieth off the stream. The paths
still rise, so the explanation above was only half of it: what a lossless coder
minds more than a denser mixture is a mixture that keeps changing.

It stays a knob — `harlettizer encode --fold-search <passes>`, four by default
and zero to decline it, with `cargo xtask cluster --search` the same knob on the
harness. A measured curve with a knee is worth being able to point at, six
passes is worse on both axes, and the mechanism that made it cost bitrate on its
own is real enough to show up again on another master.

#### A cost for moving, written and inert

An element that moves takes every object folded into it along, including the
ones that did not move — which the metric cannot see, since it judges an
instant and never a movement. So the score carries a term for how far a
candidate is from where the element was last block, weighted by what it
carries. Measured, it does nothing and then it does harm:

| cost of moving | mean | worst | travel a block | flips a block |
|---|---|---|---|---|
| **0** | **0.037** | **0.328** | **2.52°** | 2.80 |
| 0.02 | 0.037 | 0.328 | 2.52° | 2.86 |
| 0.1 | 0.037 | 0.328 | 2.53° | 3.12 |
| 0.5 | 0.037 | 0.338 | 2.57° | 4.30 |

The travel it was meant to bound is **the scene turning**, not the search
wandering: two and a half degrees a block is where the objects went, and the
search moves an element by a fraction of that. It is zero, and the term is kept
where a measurement can find it, because the case it guards against — a search
wandering while the scene is still — is one no scene here contains.

### The proxy that was tried first, and why it lost

The element positions are decided in **directions** — the members'
energy-weighted mean direction, with their mean radius handed back afterwards
— and the metric measures **gain vectors**. So the obvious improvement is to
close that gap: put each element at the cube position whose gain vector best
matches the energy-weighted mean of its members' gain vectors, over a
precomputed grid of the cube with a refinement off it.

It was built and measured. It is worse, everywhere — and the section above says
what it was measuring instead of the metric:

| Elements | Fitted, mean direction | Fitted, on the metric |
|---|---|---|
| 6 | **0.174** | 0.212 |
| 8 | **0.105** | 0.141 |
| 12 | **0.058** | 0.072 |
| 16 | **0.038** | 0.045 |

`Nearest` and `Spread` lose by less and lose too. The search is not the
problem: refining a hundred and twenty-eight times finer than the grid moves
the numbers by a thousandth — 0.212 to 0.211 at six elements — so it has
converged, and what it has converged to is worse.

The reason is that a fold is **not linear in position**. Averaging gain vectors
and then asking which position produces something pointing that way is
averaging in one space and inverting a non-linear map into another; the
position it lands on is not the position that minimises the rendering error.
Averaging the members' directions has no such step in it, and it wins.

What this does *not* rescue is the case that motivated it — the ceiling object
folded into a floor element. Nothing energy-weighted rescues that: the object
is 38 dB down, so it carries a ten-thousandth of the weight wherever the
averaging is done. It would take an element of its own, and on that master
there is a spare one; the seeding is energy-weighted too, so no quiet object
ever earns a seed. That is the lever, and it is not this one.

### An element with slack in it can be taken back

An iteration of Lloyd's method moves a centre; it never hands an element from
one part of the scene to another. So an element given to a quiet object by the
spare rule keeps it for as long as that object exists, however much better it
would be spent elsewhere later. And no figure here could report that: travel
ignores an element carrying nothing and so does the flip count, on the argument
that relocating an empty element is the feature working. That argument is right
and it leaves the case unwatched — an indicator that excludes a case by
construction is not a check on it, which is an argument this project has
already made against itself once.

One move, judged rather than argued: take the element carrying the least, put
it on the object that is worst served, re-solve every object's weights, and
keep the result only if the metric says it costs less. The score before and
after is the same quantity the metric reports, over the same stacked
presentations the weights were fitted against.

| | error mean / worst | exchanges | a block |
|---|---|---|---|
| crowd40, 12 elements, before | 0.044 / 0.377 | — | 58 µs |
| crowd40, 12 elements, after | 0.044 / 0.377 | 1 in 100 blocks | 101 µs |
| crowd40, 16 elements, before | 0.035 / 0.293 | — | 76 µs |
| crowd40, 16 elements, after | **0.033 / 0.216** | 50 in 100 blocks | 133 µs |
| ring of 48 into 16 | 0.038 / 0.308 → **0.037 / 0.213** | | |

It pays where a fold has slack in it and does nothing where every element is
contested, which is the measurement working in both directions.

#### An element under a whisper is not thereby idle

Less often than it sounds, and the reason is worth writing down. The fit is
free to send any object through any live element and does: an element overhead
is a direction a floor-level object leans on to shape what it radiates on
7.1.4. So the element sitting under a whisper is usually carrying a share of
something loud, and taking it away costs more than it saves — which is why
forty objects into twelve exchange once in a hundred blocks and into sixteen
exchange in half of them.

#### The elements nothing else counts

`xtask cluster` now reports how many elements a block carry less than a
ten-thousandth of what the loudest carries — the population every other figure
here excludes:

| | under the floor, a block |
|---|---|
| crowd40, 12 and 16 elements | 0.00 |
| prog11, 12 elements | **4.62** |

Nearly half of `prog11`'s twelve elements are below the floor at any moment,
which is what an eleven-object master folded into twelve looks like from the
inside and what no other line of the report says.

### A parked element is a direction the scene never asked for

`separate` moves an element nothing chose to somewhere a triangulation can work
with. That is a position, and a panner or a fit will happily send an object
through it — which puts that object's signal where the mix has nothing at all.
The weighting is now computed over the elements that carry something and the
rest are left at zero.

### Bounding how far an element may move makes it move further

A hysteresis stops an object changing hands over nothing; the obvious companion
is to stop the element itself bolting when it does, by capping its travel at
ten degrees a block. Built and measured, on two masters:

| | error, mean / worst | travel a block | jumps over 15° |
|---|---|---|---|
| prog11, capped | 0.000 / **1.338** | **0.45°** | 5 |
| prog11, uncapped | **0.000 / 0.000** | **0.00°** | **1** |
| cc, capped | 0.003 / 1.057 | 0.18° | **4** |
| cc, uncapped | **0.000 / 0.000** | **0.13°** | 9 |

*(These are sixteen elements, and they are history: the uncapped column does
not reproduce, because the element numbering has since been made stable — see
[An element keeps its number](#an-element-keeps-its-number) for what the same
masters give now, and at which element count. The comparison the table was
taken for is still the comparison, and the cap is still not taken.)*

It is worse at the thing it is *for*. Capping an element short of where it
should be means the next block caps it again, and the block after that: one
arrival becomes a slide that never settles, so the average travel goes **up**,
from nothing at all to nearly half a degree a block. The error goes up with it,
because an element that never arrives is an element its objects are wrong
about — and on the master with a spare element it undoes the handout entirely,
clamping the freshly given element ten degrees from the object it was given.

Not taken. What a cap would be for — an element that really does have to cross
the room — is already handled by the thing the cap was added beside: an element
that carried nothing is free to go anywhere, because nobody was listening to
where it was.

### A programme in two acts is not folded by the basins the first one chose

The first block decides which parts of the scene the elements sit in, and every
block after it is warm-started from the one before. That is what keeps an
element still when the scene has not moved, and it is also a decision never
revisited: Lloyd's method finds the nearest local answer to where it started,
so a programme that opens on a line of dialogue at the centre and closes on a
battle is folded, two hours later, by basins the dialogue chose.

Every eight blocks — a fifth of a second — the scene is settled a second time
**from cold**, and the warm answer has to go on earning what it inherited. The
cold answer is taken only when it is both worth something and cheap:

- **worth**: at least five per cent less than the warm one in the metric's own
  terms, and at least a ten-thousandth of the scene's energy in absolute ones;

- **cheap**: what it costs to get there — how far each element moves, weighted
  by what it is carrying while it goes — discounted by the number of blocks the
  saving is guaranteed for. A better answer saves its difference *every* block;
  the move is paid *once*, so comparing the two undiscounted compares a rate
  with a total.

That second term is also how a cut is noticed without looking for one: after a
cut the elements carry nothing that was there before, so moving them costs
nothing and the restart is free.

| | error mean / worst | restarts | a block |
|---|---|---|---|
| crowd40, 12 elements | 0.044 / 0.377 → 0.044 / 0.377 | 0 in 100 | 58 → 86 µs |
| crowd40, 16 elements | 0.035 / 0.293 → **0.029 / 0.293** | 1 in 100 | 76 → 112 µs |
| prog11, 12 elements | 0.000 / 0.000 → 0.000 / 0.000 | 12 in 753 | |
| cc, 12 elements | 0.000 / 0.000 → 0.000 / 0.000 | 0 | |
| two acts, second act | 0.211 → **0.185** | 1 in 40 | |

A single restart takes a sixth off `crowd40` at sixteen elements for the rest of
the run, and forty objects into twelve — where every element is contested and
no other arrangement is better — never restarts at all.

#### A fraction on its own fires on nothing

The first rule was relative only, and on a master whose fold is the identity
both answers cost about nothing: one about-nothing is five per cent less than
another on rounding alone, and `prog11` restarted in 27 of its 753 blocks
for savings in the thirtieth decimal. Nothing measurable changed — the answer
is the same answer — but a rule that fires on noise will fire on noise
somewhere it matters. With an absolute floor of a ten-thousandth of the scene's
energy under it, that falls to 12, and every figure for that master is still
exactly what it was.

### Real masters cannot tell these apart

Run over the master sets under `dumps/harlettizer/atmos/`, every weighting and
every element count scores 0.000 to 0.001 on every presentation. That is not a
result about the weightings: every one of those masters is the **output** of a
clustering — eleven to fifteen objects folding into twelve to sixteen elements
— so the answer is very nearly the identity whatever the rule. The tables above
are synthesised scenes with more objects than elements, which is the only shape
that asks the question.

### The level column is a defect the first measurement could not see

`Spread` renders objects **16 to 25 per cent too loud** — one to two decibels.
Its weights carry unit power, which is exactly what vector-base amplitude
panning guarantees and exactly what the first version of this measurement
checked. But the elements are panned *again* by whatever plays the stream, and
being correlated they add coherently, so the power that was right at the weights
is wrong at the speakers.

The metric now reports the level **as rendered**. A proxy that a weighting is
constructed to satisfy is not a check on that weighting.

### How the fitted weighting works, and the reference that does not

Rendering is linear in the object signals: element `k` carrying weight `w_k`
contributes `w_k · g(q_k)`, and the object should have contributed
`g(p, size)`. Choosing the weights is therefore a least-squares problem, and
the only thing stopping it being solved is that the encoder does not know which
layout `g` will be. So it is solved against a reference layout instead, by
non-negative least squares — non-negative because a negative weight subtracts an
object from an element other objects are already in, and cancelling solutions
fit beautifully and fall apart the moment anything moves.

**Which reference is not a detail, and the obvious choice is the wrong one.**
Fitted against twenty-four directions spread evenly over the sphere and measured
on 7.1.4, a wide object costs 0.784 — barely better than ignoring its width.
Fitted against the directions of an ordinary 7.1.4 and measured on layouts that
are *not* that one:

| | Fitted | Nearest |
|---|---|---|
| a point, on 5.1 | 0.530 | 0.539 |
| a point, on 7.1 | 0.530 | 0.539 |
| a width, on 5.1 | **0.121** | 0.633 |
| a width, on 7.1 | **0.137** | 0.669 |

Panning with a width is not the same operator on two different layouts, so a fit
made on an idealised sphere is not a fit on a real room. An encoder cannot know
what is in the room, but assuming something ordinary transfers; assuming
something uniform does not.

A least-squares fit is also *shorter* than its target whenever the target is
unreachable, so the raw fit was eleven per cent quiet. The shape is what is
worth keeping, so it is kept and the level put back.

### How well the fold could be done at all

If the encoder knew the layout, the best possible fold of a ring of sixteen into
twelve costs 0.530 for a point object and 0.237 for a wide one. That is the
floor, it is not achievable, and it is the number that says whether a rule is
bad or a problem is hard.

Fitting reaches **both** — 0.530 and 0.237 — from an assumption about the layout
rather than knowledge of it. `Nearest` reaches the floor for points (0.539) and
misses it by a factor of 2.6 for widths.

**A width is easier to fold than a point**, which is worth stating because it is
the opposite of what one expects. A broad target is something a handful of point
elements can add up to. A narrow one sitting between them is not: all you can do
is pan between them, which makes it broad.

## What the two direction-based weightings cost

An object that does not sit on an element either gets **moved** to one or gets
**spread** across several, and both cost something:

- **Spread** — pan it onto the elements. Its direction is preserved exactly;
  its energy lands on two or three elements which are then panned *again* by
  whatever plays the stream. Panning twice is wider than panning once.
- **Nearest** — give it to one element outright. Nothing is spread and nothing
  is panned twice; the object arrives at the element's direction instead of its
  own.

Measured on a synthesised scene of 48 moving objects, folded and rendered onto
7.1.4:

| Elements | Spread — mean / worst | Nearest — mean / worst |
|---|---|---|
| 12 | 0.239 / **0.538** | **0.175** / 0.628 |
| 16 | 0.222 / **0.539** | **0.128** / 0.549 |

Neither wins outright against the other: nearest is better for most of the
scene, spread bounds the worst-served object. Both are beaten on both counts by
fitting, which is why they are kept for comparison rather than as choices.

## The elements have to stay still

The first measurement of a scene turning by three degrees: eleven of twelve
elements followed it by exactly three degrees, and the twelfth moved **nine**,
because one object crossed a boundary and dragged its element after it. What a
listener hears from that is the jump, not the mix.

So an object keeps its element unless another is better by more than five
degrees, and a block inherits the previous block's assignment rather than only
its positions. With that, all twelve follow the turn by exactly three degrees,
and a 48-object scene over 150 blocks moves its elements 0.27° a block with **no
jump over fifteen degrees at all**.

### Five degrees was 0.87° where it mattered

The comparison is made in cosines, because that is what the assignment is made
in, and the margin was a fixed `1 − cos 5° = 0.0038` subtracted from it. That is
five degrees only where the challenging element sits on top of the object. Away
from there the cosine flattens, and the same margin is worth about
`0.0038 / sin θ` radians at an angle θ from the element an object is on:

| an object this far from its element | a fixed cosine margin is worth |
|---|---|
| 5° | 5° |
| 15° | 0.87° |
| 30° | 0.44° |
| 60° | 0.25° |

Fifteen degrees is where a boundary object sits with twelve elements — and the
boundary object is the only kind that ever changes hands. So the guard was
almost absent exactly where it is for. Measured directly, by walking a
challenger in on an object fifteen degrees from its element: it gave way at
**0.87° closer**, against the five degrees the constant says and the comment
claimed.

None of the scenes here showed it. A ring that turns smoothly moves every object
the same way at the same time, so nothing crosses a boundary that was not going
to cross it anyway — `crowd40` reports the same 0.044 / 0.377 error, the same
2.51° of travel, the same 36 jumps and the same 4.74 flips either way. What
shows it is a scene that does not move at all: 48 objects held at fixed places
and displaced by a random angle each block, folded into twelve elements over a
hundred blocks. Every displacement is inside the five degrees the margin claims,
so the right answer is that nothing changes hands.

| objects displaced by | a fixed cosine margin | at the angle the object is at |
|---|---|---|
| 1° | 3 | **0** |
| 3° | 62 | **5** |
| 4° | 235 | **20** |

The margin is now taken at the angle the object is actually at: a challenger has
to be inside `θ − 5°`, which in cosines is `cos θ cos 5° + sin θ sin 5°` — one
square root per object per pass, no inverse cosine, and no measurable cost
(73 µs a block against 78 µs before, which is noise). An object already inside
the margin keeps its element outright, since no angle is five degrees less than
three.

## Real trajectories broke it on the first attempt

Everything above was measured on synthesised scenes — objects walking circles,
never two of them in the same place. The first run against *authored* movement,
taken from a real immersive stream decoded back to a master set, failed
outright:

```text
xtask: cluster positions: not supported yet: find_ls_triplets failed
```

A localisation test puts every object at the front or the back and nothing in
between. Two distinct directions and twelve elements to spend leaves ten of them
piled on top of each other, and a triangulation over coincident points has no
triplets to find. Circles never showed it, because objects on a circle are never
coincident.

So an element the scene did not ask for is moved somewhere the geometry can use
— and *which* one moves is decided by what it carries, because moving an element
with energy in it moves what a listener hears. That is also why the stability
figure counts only elements carrying something: relocating an empty one is the
feature working, and counting it would report a feature as a fault.

The same scene now folds without complaint, exactly (its fifteen objects have
two directions between them, so twelve elements is not a fold at all), with its
elements moving 0.09° a block and no jumps.

The lesson is not the bug. It is that a synthesised test set agrees with
whatever you assumed when you wrote it.

## What it costs to run

A whole programme is the only length that decides whether an encoder can do
this at all. Measured on the real path — `cargo xtask cluster` reports it —
folding into twelve elements at twenty-five metadata blocks a second,
extrapolated to two hours:

| Objects | Per block | A two-hour programme |
|---|---|---|
| 16 | 8.8 µs | 1.6 s |
| 48 | 29.7 µs | 5.3 s |
| 100 | 59.6 µs | 10.7 s |
| 150 | 93.7 µs | 16.9 s |

Linear in the objects, at 0.63 µs each, with no fixed cost worth naming. More
elements cost about 6 µs a block each: 15.4 s for a programme at sixteen.

Where it goes, at a hundred objects: **9 µs** is the clustering itself, **32 µs**
is panning each object and element onto the reference, and **18 µs** is the
solve. The least-squares fit — the part that looked expensive — is under a third
of it.

**Against the encoder it feeds**, which is the comparison that settles it: the
TrueHD encoder in this same workspace takes **21.5 minutes** of processor time
for a two-hour sixteen-channel programme. The clustering is 0.8 % of that.

### One thing that was not free

The reference the fit solves against is a triangulation of a fixed set of
directions, and it was being rebuilt **every block** — nothing about it depends
on the scene. That was 24 µs a block: a third of the work at sixteen objects,
and pure waste at any count. Held across the blocks instead, as an encoder would,
a sixteen-object scene went from 34.7 µs a block to 8.8.

That is what `Clusterer` is for. `cluster()` is still there and still builds one
each time, which is right for a single call and wrong for a programme.

## Nothing shipped carries an object's size

Counted across every master to hand:

| Field | Values seen |
|---|---|
| `size` | `0.0`, every time |
| `size3D` | never stated |
| `decorr` | never stated |
| `zones` | `all`, always |
| `snap` | `false`, always |
| `elevation` | `true`, always |
| `screenFactor` | `0.0`, always |
| `depthFactor` | `0.25`, always |

Only position and gain ever move. The format *can* state a size — the field is
there and this project writes it — so this is a choice the reference encoder
makes, not a limit it is under. Why it makes it is not answerable from here;
whether a given playback device would honour a size if it got one is not
answerable from here either, and nothing above should be read as saying it
would.

What follows for the encoder does not depend on the reason. **A mix uses object
size, and the output will not carry it, so the fold is the only place left for
it to go**: a wide object has to become energy spread across several elements,
because that is the only thing the delivery format can represent.

## The measurement can now see a width, and says nothing carries it

Until this was noticed, the metric panned every object as a point — so a fold
that flattened a wide object to a single element scored exactly as well as one
that carried it. It now pans the object with its size and the elements without,
which is what the delivery format does.

The first thing it said, on a ring of sixteen objects folded into twelve:

| | Elements reached | Cost |
|---|---|---|
| The same object as a point | 3 | 0.638 |
| Spread, once it has a width | 12 | **0.898** |
| Nearest, once it has a width | 1 | **0.613** |

**Neither rule carries a width.** Panning onto the elements preserves a
*point's* direction and energy, which is not the same as preserving a cone, and
twelve irregularly placed elements are not a grid to spread over. Flattening
scores better here only because spreading over the wrong set is worse than not
spreading at all.

That target — a width should cost no more than the same object as a point — is
now met, and beaten: fitted, a width costs *less*. See above.

## One scene, built once

The energy an object carries steers everything: which objects earn a seed, how
hard each pulls its element, which of them the metric's forty-decibel floor
lets set a worst case, and which the report counts at all. **Two commands were
computing it, and not the same way** — `cargo xtask cluster` multiplied in the
master's `importance` and `harlettizer encode` did not, so the harness was
reporting on a scene the stream it described had never folded.

It is now built in one place, `hz_cluster::scene`, and both of them call it.
No master to hand states an importance other than the top rank, so no figure
here moves; what moves is that the two can be pointed at the same master
and compared at all. `xtask cluster` gained `--block` for exactly that, since
the encoder's block is 1280 samples and no whole number of blocks a second is:

```text
cargo xtask cluster crowd40.atmos --elements 11 --block 1280
  error        0.068 mean over the presentations, 0.478 at its worst

harlettizer encode crowd40.atmos --out crowd40.thd --cluster 12
  fold         0.0682 mean over the presentations, 0.478 at its worst
```

Eleven against twelve because the low frequency channel takes an element of its
own and is not clustered.

### A physical power is not what a listener weighs

The energy is a mean square, which is a meter's answer. A rumble at −10 dBFS
outweighs a line of dialogue at −20 dBFS by ten to one in it, and it is the
dialogue a listener localises — so the rumble takes the seed, pulls the centre,
sets the floor under the worst case, and the voice is folded into whatever
happens to share its azimuth.

The correction is standard and already in the tree: take the block's power
through the K-weighting filter pair of ITU-R BS.1770, the same one
`hz_analysis` measures loudness with. Measured on the design at 48 kHz it is
−13.3 dB at 20 Hz, −8.3 dB at 30 Hz, −5.6 dB at 40 Hz, −1.1 dB at 100 Hz,
+0.7 dB at 1 kHz, +3.1 dB at 2 kHz and +4.0 dB from 4 kHz up.

**It is not switched on, because the metric says it costs.** On `crowd40` into
twelve elements, measured on a fixed yardstick so that a change in the rule is
not hidden by a change in the ruler:

| steered by | mean error | worst | flips a block |
|---|---|---|---|
| the plain power | **0.044** | **0.377** | 4.74 |
| the K-weighted power | 0.087 | 0.839 | **3.15** |

Twice the error for a third off the flips. The argument for taking it anyway —
that the objects it gives up on are ones nobody was localising — is exactly the
kind of argument that is not accepted here without a measurement, and `crowd40`
is the worst scene on which to settle it: forty objects, each a pure tone,
spread over five octaves, so a perceptual re-weighting moves their ranking by
seventeen decibels where broadband content would move it by two or three.
Neither yardstick decides it, and the metric cannot judge it by ear, so the
rule that costs nothing stays and the other one stays compiled, tested and one
constant away — `hz_cluster::scene::LOUDNESS`. The broadband scene below,
built to be the fairer judge, agrees with `crowd40` about it: 0.076 against
0.064 at twelve elements on the plain ruler, and worse on the perceptual one
too.

### What its disappearance would change: a perceptual importance, measured

K-weighting re-weighs a rumble against a voice and stops. The quality actually
wanted is different: an object's weight is **what its disappearance would
change** in what is heard, which is a partial loudness with masking, band by
band. It was built, and it was measured on both rulers. It does not earn the
default, and this section is the record of why.

**What was built.** Three things, each in the place it belongs:

- `hz_analysis::bands` splits a block into equal steps of the ERB-rate scale
  of Glasberg and Moore — one transform of the block, each bin weighted by the
  K filter's own magnitude and summed into the band it falls in. It carries no
  state between blocks, so an analysis is a function of the block alone. The
  bands sum to the K-weighted mean square, which is the unit the rest of the
  fold already speaks.
- `hz_analysis::partial` is the compressive law of specific loudness,
  `(E + A)^α − A^α`, with the fifth-power exponent and the threshold term of
  ISO 532-2, and what a sound **adds**: the loudness of a band with it in, less
  the loudness without it.
- `hz_cluster::scene` puts them together. An object's energy under
  `Loudness::Perceptual` is what it adds to the scene, summed over the bands,
  and **the masker is spatial**: what masks object `i` is every other object
  weighted by the overlap of its rendered gain vector with `i`'s over the four
  presentations — the metric's own language, so two objects the same way round
  overlap completely and two on opposite walls hardly at all. A `Global`
  masker weighs every other object by one, and both were measured.

The excitation the law needs is met by a stated calibration — full scale at
105 dB SPL, SMPTE RP 200's — which decides only where the threshold term falls,
a hundred decibels down; deriving it from the programme's own dialogue level is
a separate lever.

**The ruler changes with the rule.** The metric weighs each object's error by
the energy the fold was steered by, so a rule that changes the weight changes
the judge, and a fold that looks better under its own rule may only be
measuring itself more kindly. `cargo xtask cluster` therefore judges every
fold twice — by the rule it was steered by and by the other one — and prints
both. What a rule *gains* is what moves on its own ruler between a fold
steered the old way and one steered the new way; what it *costs* is what
moves on the old ruler.

**A fairer scene.** `crowd40` is forty pure tones over five octaves, and any
frequency weighting moves their ranking by tens of decibels. So a second scene
was built with the same trajectories and different contents —
`cargo xtask make-master --scene broadband`: one object is a voice, noise
shaped to the band a voice occupies with a syllabic rise and fall at −20 dBFS;
one is a rumble, nothing above sixty hertz at −10 dBFS, the loudest thing in
the scene to a meter; the other thirty-eight are effects, broad noise centred
at places spread evenly over the ERB-rate scale from 150 Hz to 12 kHz, their
levels spread over sixteen decibels, every third of them given a width of 0.3.
It is `broad40` below, and it is the harder scene: 0.064 against 0.038 at
twelve elements under the same rule.

**What it measured.** Four tables, one per scene and element count; the
encoder's block of 1280 samples, four passes of placement, everything else as
it ships. Each perceptual configuration is shown against the plain fold judged
on *that* configuration's ruler, since the ruler moves with the band count as
well.

**`crowd40`, 12 elements.** Steered by the plain power the fold costs 0.038 / 0.308 on its own ruler, 1.76 flips a block.

| steered by | bands | masker | on the perceptual ruler | on the plain ruler | worst | flips a block |
|---|---|---|---|---|---|---|
| the plain power | 1 | global | 0.039 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 1 | global | **0.044** | 0.045 | 0.345 | 2.16 |
| the plain power | 3 | global | 0.037 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 3 | global | **0.038** | 0.041 | 0.340 | 1.99 |
| the plain power | 6 | global | 0.037 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 6 | global | **0.038** | 0.040 | 0.329 | 1.78 |
| the plain power | 12 | global | 0.032 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 12 | global | **0.030** | 0.042 | 0.357 | 2.17 |
| the plain power | 24 | global | 0.034 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 24 | global | **0.033** | 0.056 | 0.545 | 2.50 |
| the plain power | 1 | local | 0.040 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 1 | local | **0.045** | 0.046 | 0.339 | 2.44 |
| the plain power | 3 | local | 0.038 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 3 | local | **0.043** | 0.044 | 0.283 | 2.29 |
| the plain power | 6 | local | 0.037 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 6 | local | **0.040** | 0.041 | 0.279 | 2.19 |
| the plain power | 12 | local | 0.033 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 12 | local | **0.033** | 0.044 | 0.385 | 2.33 |
| the plain power | 24 | local | 0.034 | **0.038** | 0.308 | 1.76 |
| the perceptual importance | 24 | local | **0.032** | 0.054 | 0.516 | 3.24 |
| the K-weighted power | 6 | local | 0.052 | 0.044 | 0.345 | 2.18 |

**`crowd40`, 16 elements.** Steered by the plain power the fold costs 0.024 / 0.233 on its own ruler, 3.26 flips a block.

| steered by | bands | masker | on the perceptual ruler | on the plain ruler | worst | flips a block |
|---|---|---|---|---|---|---|
| the plain power | 1 | global | 0.024 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 1 | global | **0.028** | 0.030 | 0.239 | 1.58 |
| the plain power | 3 | global | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 3 | global | **0.027** | 0.028 | 0.301 | 2.65 |
| the plain power | 6 | global | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 6 | global | **0.024** | 0.027 | 0.260 | 2.79 |
| the plain power | 12 | global | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 12 | global | **0.018** | 0.026 | 0.215 | 2.53 |
| the plain power | 24 | global | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 24 | global | **0.021** | 0.031 | 0.320 | 3.34 |
| the plain power | 1 | local | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 1 | local | **0.025** | 0.025 | 0.291 | 1.57 |
| the plain power | 3 | local | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 3 | local | **0.027** | 0.026 | 0.253 | 2.07 |
| the plain power | 6 | local | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 6 | local | **0.023** | 0.025 | 0.269 | 2.44 |
| the plain power | 12 | local | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 12 | local | **0.021** | 0.028 | 0.283 | 2.35 |
| the plain power | 24 | local | 0.025 | **0.024** | 0.233 | 3.26 |
| the perceptual importance | 24 | local | **0.022** | 0.030 | 0.262 | 3.35 |
| the K-weighted power | 6 | local | 0.033 | 0.028 | 0.296 | 2.86 |

**`broad40`, 12 elements.** Steered by the plain power the fold costs 0.064 / 0.613 on its own ruler, 3.13 flips a block.

| steered by | bands | masker | on the perceptual ruler | on the plain ruler | worst | flips a block |
|---|---|---|---|---|---|---|
| the plain power | 1 | global | 0.072 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 1 | global | **0.077** | 0.075 | 0.625 | 3.45 |
| the plain power | 3 | global | 0.072 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 3 | global | **0.076** | 0.072 | 0.627 | 2.94 |
| the plain power | 6 | global | 0.069 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 6 | global | **0.073** | 0.071 | 0.594 | 2.72 |
| the plain power | 12 | global | 0.067 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 12 | global | **0.072** | 0.070 | 0.631 | 3.23 |
| the plain power | 24 | global | 0.067 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 24 | global | **0.071** | 0.070 | 0.596 | 2.84 |
| the plain power | 1 | local | 0.072 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 1 | local | **0.074** | 0.072 | 0.595 | 3.00 |
| the plain power | 3 | local | 0.074 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 3 | local | **0.075** | 0.070 | 0.594 | 2.85 |
| the plain power | 6 | local | 0.072 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 6 | local | **0.073** | 0.069 | 0.632 | 2.83 |
| the plain power | 12 | local | 0.071 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 12 | local | **0.072** | 0.069 | 0.631 | 2.81 |
| the plain power | 24 | local | 0.071 | **0.064** | 0.613 | 3.13 |
| the perceptual importance | 24 | local | **0.072** | 0.069 | 0.631 | 2.79 |
| the K-weighted power | 6 | local | 0.078 | 0.076 | 0.627 | 3.33 |

**`broad40`, 16 elements.** Steered by the plain power the fold costs 0.055 / 0.607 on its own ruler, 3.38 flips a block.

| steered by | bands | masker | on the perceptual ruler | on the plain ruler | worst | flips a block |
|---|---|---|---|---|---|---|
| the plain power | 1 | global | 0.062 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 1 | global | **0.061** | 0.057 | 0.672 | 3.49 |
| the plain power | 3 | global | 0.062 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 3 | global | **0.060** | 0.055 | 0.592 | 4.09 |
| the plain power | 6 | global | 0.059 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 6 | global | **0.057** | 0.054 | 0.615 | 4.27 |
| the plain power | 12 | global | 0.057 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 12 | global | **0.057** | 0.055 | 0.616 | 4.43 |
| the plain power | 24 | global | 0.057 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 24 | global | **0.057** | 0.056 | 0.608 | 3.85 |
| the plain power | 1 | local | 0.062 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 1 | local | **0.058** | 0.053 | 0.607 | 3.49 |
| the plain power | 3 | local | 0.064 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 3 | local | **0.059** | 0.053 | 0.610 | 3.64 |
| the plain power | 6 | local | 0.062 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 6 | local | **0.058** | 0.054 | 0.609 | 4.03 |
| the plain power | 12 | local | 0.061 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 12 | local | **0.057** | 0.054 | 0.609 | 4.17 |
| the plain power | 24 | local | 0.060 | **0.055** | 0.607 | 3.38 |
| the perceptual importance | 24 | local | **0.057** | 0.055 | 0.607 | 4.06 |
| the K-weighted power | 6 | local | 0.058 | 0.060 | 0.598 | 3.89 |

Read down the "on the plain ruler" column first: **no configuration improves
on the plain fold on the plain ruler on any table**, but for a thousandth or
two on `broad40` at sixteen elements, and the rest cost between four and
twenty-two per cent. Then the other column: on its own ruler the rule gains on
`crowd40` at sixteen elements — 0.025 to 0.018 with twelve global bands, a
quarter off the mean and the worst object with it — and *loses* on `broad40`
at twelve, where every configuration is worse than the plain fold even by its
own account. Averaged over the four tables, per configuration:

| bands | masker | on its own ruler | on the plain ruler | worst | flips |
|---|---|---|---|---|---|
| 1 | global | +8.7 % | +16.1 % | +6.8 % | −3.8 % |
| 3 | global | +3.3 % | +9.3 % | +9.8 % | +2.3 % |
| 6 | global | +0.3 % | +6.7 % | +4.2 % | 0.0 % |
| 12 | global | −6.7 % | +7.1 % | +3.2 % | +8.8 % |
| 24 | global | −3.2 % | +21.9 % | +27.9 % | +12.3 % |
| 1 | local | +2.2 % | +8.5 % | +8.0 % | −3.5 % |
| 3 | local | +3.7 % | +7.5 % | −0.5 % | −1.9 % |
| **6** | **local** | **−1.2 %** | **+4.5 %** | **+2.4 %** | **+2.2 %** |
| 12 | local | −5.3 % | +9.6 % | +12.4 % | +4.4 % |
| 24 | local | −5.4 % | +18.7 % | +20.7 % | +24.0 % |

Six bands with the local masker is the configuration that moves least on
every axis, which is not the same as winning; it is what
`hz_cluster::scene::PERCEPTION` names, so that the other ruler the harness
prints is a fixed one. Twenty-four bands is the one to point at for what goes
wrong with more resolution: a band a bin or two wide at the bottom of the
scale measures where the block boundary fell as much as what the object did,
and the flips say so.

**Not taken.** The rule does not beat the plain one on the plain ruler
anywhere, beats it on its own ruler on two tables of four and loses on a
third, and no scene here has been listened to. That is a tie at best, and
rule 4 of `docs/clustering-next.md` says what settles a tie. The plain power
stays; `Loudness::Perceptual` stays compiled, tested, measurable from the
harness with `--loudness perceptual --bands N --masker global|local`, and one
constant away — and, since a tie is settled by listening, `harlettizer encode
--loudness perceptual` steers a fold by it as measured, six bands and the
local masker, so that a programme can be encoded both ways and heard. The
summary says which rule a stream was folded under. The two masters whose fold is the identity report 0.000 under
either rule, and `harlettizer encode` is byte for byte what it was, on
`crowd40` and on `prog11`, since the constant it folds by did not move.

**What it costs, and would have to stop costing.** The plain scene is 22 µs a
block for forty-one channels; the perceptual one is 560 to 610 µs, a
transform of 2048 points per object per block. Over a two-hour programme at
the encoder's block that is 200 s of analysis, and the encoder as written
analyses every span twice — once ahead, once to fold — so 6.6 minutes against
the 21.5 the encoder itself takes: thirty per cent, against a budget of a few.
Two halvings are known and neither was made, since nothing was taken that
would need them: the transform is complex and the input is real, so two
objects can share one transform; and the analysis carries no state, so the
span analysed ahead is the span folded next and need not be analysed again.
Eight per cent is where those would land it.

**Three things worth knowing that came out of it.**

*The marginal loudness of a non-dominant object is nearly its power.* One band
with the global masker reproduces the K-weighted fold to the digit — 0.044 /
0.345 both ways on `crowd40` at twelve. That is the law's derivative at work:
what an object adds to a masker much larger than itself is the slope of the
law times its power, and in a forty-object scene the masker is nearly the
whole scene for everyone but the loudest few. The compression discounts the
dominant objects and leaves the ranking of the rest where the power put it.
So a per-band model is the K-weighting plus a masking discount, and the
masking discount is what the tables above measure.

*A loud object's importance is discounted by what is under it.* By the
definition — the loudness with it in, less the loudness without — an object
thirty-eight decibels under another in the same band still has a seventh of
its loudness under the compressive law, and takes that seventh off the loud
one's importance. `masking_is_by_band` in `hz_cluster::scene` states it. It is
the definition and not a defect, and it is one of the ways the rule's ruler
differs from the plain one.

*The compression alone halves the flips at sixteen elements.* One band, no
masking between bands: 3.26 flips a block to 1.58 on `crowd40`, 1.57 with
the local masker — for eight per cent on its own ruler and sixteen on the
plain one. A weight that is a fifth power of a power ripples a fifth as much
with where the block boundary falls, which is exactly the tremor the second
lever of `docs/clustering-next.md` is about. That lever is where this
observation belongs, and it is not a reason to take a rule that costs error.

One knob was not re-tuned for the new unit and should be, if this is ever
measured again: the anticipation places an object for the next block when
that block's energy is at least twice this one's, and twice a *loudness* is
fifteen decibels of power rather than three. Under the perceptual rule the
anticipation therefore fires only on an object appearing from silence, which
is the case it was written for and not the only case it caught.

## An object that appears is carried from its first sample

A block's elements are placed from the scene at its **end** and reached at its
end, because a decoder ramps a position across the access unit the payload
rides. That is right for movement: an object crossing the block is somewhere
between where it was and where it is going, and so is the element carrying it.

It is wrong for an object that *appears*. Silent through block `t−1` it pulled
nothing, so no element is near it; loud from the first sample of block `t` it
is carried by elements that only arrive where it is as that block ends. The
attack — the part of a sound a listener localises — is rendered by elements in
transit. Measured on twenty objects turning with one switching on hard: **the
object that appears lands 1.25 out of its own gain vector as its first block
arrives**, which is a whole object somewhere else.

### A block ahead is not a look-ahead, it is a lead

The obvious remedy is to cluster block `t` on the scene of `t+1`. That puts
every element a whole block in front of the audio, and the metric says so. The
same scene, judged twice — as each block **arrives** (objects where they are at
its first sample, elements where the last payload left them) and as it
**departs** (objects at its last sample, elements at this payload's target),
both carrying the energies of the block that is playing:

| the scene turning | placed for | on arrival | on departure |
|---|---|---|---|
| 2° a block | this block | 0.0640 | 0.0631 |
| | the next block | 0.0823 | 0.0820 |
| 6° a block | this block | 0.0652 | 0.0644 |
| | the next block | 0.1394 | 0.1381 |

It buys the attack by paying for every object that merely moves — twice the
error everywhere on a scene turning six degrees a block.

### Only the energy looks ahead

The geometry stays where it was, aligned with the audio, and only the *energy*
anticipates: an object is placed for the next block's energy when that is at
least **twice** this block's, and for its own otherwise.

| the scene turning | placed for | on arrival | on departure | worst on arrival |
|---|---|---|---|---|
| 2°, nothing appears | this block | 0.0640 | 0.0631 | 0.746 |
| | anticipating | 0.0640 | 0.0631 | 0.746 |
| 6°, nothing appears | this block | 0.0652 | 0.0644 | 0.746 |
| | anticipating | 0.0652 | 0.0644 | 0.746 |
| 2°, one appears | this block | 0.0720 | 0.0687 | 1.312 |
| | anticipating | 0.0698 | 0.0682 | **0.746** |
| 6°, one appears | this block | 0.0715 | 0.0682 | 1.247 |
| | anticipating | 0.0719 | 0.0717 | **0.746** |

Nothing when nothing appears, and when something does, the object that appears
stops setting the worst case: 0.746 is what the rest of the scene already cost.
The price is on the last row — holding a decaying object one block longer than
it deserves, which is the same rule working, since an element should not walk
away from a sound the instant it stops.

### Why a threshold and not the louder of the two

A block's power is a mean square over 26.7 ms and a steady tone's is not
steady: it ripples with where the block boundary falls in the waveform. Taking
the larger of the two unconditionally therefore over-weights whichever objects
ripple most, which is a bias on every block rather than an anticipation on one:

| crowd40, 12 elements, placed for | mean error | worst | flips a block |
|---|---|---|---|
| this block | **0.044** | 0.377 | 4.74 |
| the louder of the two | 0.048 | 0.377 | 1.24 |
| **a doubling or more** | **0.044** | **0.377** | **4.74** |

The flip count falling to 1.24 in the middle row looks like a win and is the
ripple being smoothed, not the fold being steadier — the cost showing up as a
benefit, which is why the error is read beside it. At three decibels nothing on
`crowd40` qualifies and every figure is the unchanged one; an object appearing
out of silence is a rise of many orders of magnitude and is caught by any
threshold at all. `crowd40`, `prog11` and `cc` all report exactly what they
did before, and the encoded stream is byte for byte the same.

### What it costs to run

Some spans of latency in the encoder, in the *energy* only — as many as the
window below reaches ahead: a span is read, what its objects do in it is
measured and recorded, and the span that many behind it is folded knowing
what follows. One more span of source in memory than the look-ahead, traded
between the read buffer and the held ones. No extra clustering, no extra pass
over the audio — the energy of a span is computed once whether it is read
early or not.

## The energies are steadied over a window, and the window is two blocks ahead

The anticipation above handles one case of a more general defect. A block's
energy is a mean square over 26.7 ms and a steady sound's is not steady: it
ripples with where the block boundary falls in the waveform. The fit is handed
those energies as weights and answers a slightly different question every
block, and the flips — an element going from something to nothing between two
blocks — are partly that tremor showing through. An onset is the one case the
anticipation catches; a sound that dips for a block, or stops and starts, is
the same tremor at a larger scale, and nothing caught it.

**What was built.** `hz_cluster::smooth`: the energies the placement is
steered by are read from a **window of blocks around the one being placed**,
some behind and some ahead, and the anticipation as it was is the one-block
case of it. This encoder is offline and already reads ahead of the span it
folds, so the window can be centred on the block rather than trailing it, which
a real-time encoder could not do. Two ways of reading the window were written:
a **hold** — the loudest the object is anywhere in the window, with the blocks
ahead counting only when they are louder by more than the ripple, which is the
guard the anticipation already had — and a **mean**. Only the energy is
smoothed; the geometry a block is placed with is its own, aligned with the
audio, since leading the geometry was measured at twice the error above. The
encoder queues as many spans as the window reaches ahead, recording each span's
energies as it is read; the harness has the same window under `--smooth`,
`--behind`, `--ahead` and `--arriving`, and now reports the error **as each
block arrives** beside the error as it departs — the objects where the last
block left them, the elements where the last payload did, and this block's
energies, which is what is loud while the elements are still in transit.

**A scene that starts and stops.** Nothing in `crowd40` or `broad40` ever
starts or stops: both turn smoothly, and the only onset in either is the
voice's syllable. So a third scene was built, `cargo xtask make-master --scene
bursts` — the broadband scene with its thirty-eight effects gated on and off
for stretches drawn between eighty milliseconds and four tenths of a second,
three to fifteen blocks of the fold, with a two-millisecond ramp at every edge.
It is `burst40` below, and it is where this lever is measured: under the rule
as it was it flips 6.62 weights a block at twelve elements against 1.76 on
`crowd40`, its elements travel 4.2° a block, and its worst object as a block
arrives is 0.949 — a whole object somewhere else — where it departs at 0.919.

**What it measured.** The guarded hold, over every window from nothing to
four blocks each way, on the three scenes at twelve and sixteen elements. In
each cell: mean error over the presentations, worst object, flips a block —
and on `burst40` the worst object as the block arrives. The anticipation as it
was is in italics; what ships is in bold.

**`crowd40`, 12 elements** — mean error, worst object, flips a block, and on `burst40` the worst object as the block arrives. The anticipation as it was is nought behind and one ahead.

| behind \ ahead | 0 | 1 | 2 | 4 |
|---|---|---|---|---|
| 0 | 0.0380 / 0.308 / 1.76 | *0.0380 / 0.308 / 1.76* | **0.0380 / 0.308 / 1.76** | 0.0380 / 0.308 / 1.76 |
| 1 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 |
| 2 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 |
| 4 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 | 0.0400 / 0.315 / 1.59 |

**`crowd40`, 16 elements** — mean error, worst object, flips a block, and on `burst40` the worst object as the block arrives. The anticipation as it was is nought behind and one ahead.

| behind \ ahead | 0 | 1 | 2 | 4 |
|---|---|---|---|---|
| 0 | 0.0240 / 0.233 / 3.26 | *0.0240 / 0.233 / 3.26* | **0.0240 / 0.233 / 3.26** | 0.0240 / 0.233 / 3.26 |
| 1 | 0.0230 / 0.233 / 2.30 | 0.0230 / 0.233 / 2.30 | 0.0230 / 0.233 / 2.30 | 0.0230 / 0.233 / 2.30 |
| 2 | 0.0230 / 0.233 / 2.19 | 0.0230 / 0.233 / 2.19 | 0.0230 / 0.233 / 2.19 | 0.0230 / 0.233 / 2.19 |
| 4 | 0.0230 / 0.233 / 2.07 | 0.0230 / 0.233 / 2.07 | 0.0230 / 0.233 / 2.07 | 0.0230 / 0.233 / 2.07 |

**`broad40`, 12 elements** — mean error, worst object, flips a block, and on `burst40` the worst object as the block arrives. The anticipation as it was is nought behind and one ahead.

| behind \ ahead | 0 | 1 | 2 | 4 |
|---|---|---|---|---|
| 0 | 0.0660 / 0.613 / 3.87 | *0.0640 / 0.613 / 3.13* | **0.0640 / 0.613 / 3.08** | 0.0650 / 0.613 / 2.93 |
| 1 | 0.0650 / 0.646 / 3.24 | 0.0650 / 0.646 / 3.02 | 0.0650 / 0.650 / 3.05 | 0.0660 / 0.663 / 2.65 |
| 2 | 0.0670 / 0.686 / 3.55 | 0.0670 / 0.696 / 3.49 | 0.0650 / 0.646 / 2.73 | 0.0660 / 0.646 / 3.25 |
| 4 | 0.0670 / 0.673 / 3.65 | 0.0660 / 0.673 / 3.77 | 0.0660 / 0.673 / 3.80 | 0.0660 / 0.673 / 3.72 |

**`broad40`, 16 elements** — mean error, worst object, flips a block, and on `burst40` the worst object as the block arrives. The anticipation as it was is nought behind and one ahead.

| behind \ ahead | 0 | 1 | 2 | 4 |
|---|---|---|---|---|
| 0 | 0.0550 / 0.607 / 3.54 | *0.0550 / 0.607 / 3.38* | **0.0550 / 0.607 / 3.56** | 0.0550 / 0.607 / 3.85 |
| 1 | 0.0550 / 0.609 / 3.25 | 0.0550 / 0.609 / 3.37 | 0.0550 / 0.609 / 3.59 | 0.0550 / 0.609 / 3.32 |
| 2 | 0.0540 / 0.608 / 3.35 | 0.0540 / 0.608 / 3.23 | 0.0540 / 0.608 / 3.09 | 0.0550 / 0.608 / 2.95 |
| 4 | 0.0550 / 0.607 / 2.77 | 0.0550 / 0.607 / 3.21 | 0.0550 / 0.607 / 2.97 | 0.0550 / 0.607 / 2.74 |

**`burst40`, 12 elements** — mean error, worst object, flips a block, and on `burst40` the worst object as the block arrives. The anticipation as it was is nought behind and one ahead.

| behind \ ahead | 0 | 1 | 2 | 4 |
|---|---|---|---|---|
| 0 | 0.0510 / 0.950 / 7.27 / 0.974 | *0.0510 / 0.919 / 6.62 / 0.949* | **0.0490 / 0.916 / 4.85 / 0.700** | 0.0530 / 0.918 / 3.14 / 0.638 |
| 1 | 0.0510 / 0.948 / 6.71 / 0.948 | 0.0520 / 0.724 / 6.63 / 0.746 | 0.0530 / 0.916 / 4.52 / 0.699 | 0.0550 / 0.918 / 4.11 / 0.641 |
| 2 | 0.0510 / 0.913 / 5.12 / 0.913 | 0.0530 / 0.701 / 4.88 / 0.746 | 0.0520 / 0.916 / 4.25 / 0.711 | 0.0540 / 0.918 / 3.31 / 0.655 |
| 4 | 0.0530 / 0.771 / 3.43 / 0.771 | 0.0540 / 0.690 / 3.53 / 0.722 | 0.0550 / 0.916 / 3.20 / 0.701 | 0.0570 / 0.918 / 2.94 / 0.653 |

**`burst40`, 16 elements** — mean error, worst object, flips a block, and on `burst40` the worst object as the block arrives. The anticipation as it was is nought behind and one ahead.

| behind \ ahead | 0 | 1 | 2 | 4 |
|---|---|---|---|---|
| 0 | 0.0410 / 0.949 / 8.30 / 0.975 | *0.0410 / 0.657 / 7.33 / 0.658* | **0.0420 / 0.688 / 6.87 / 0.688** | 0.0430 / 0.661 / 3.99 / 0.661 |
| 1 | 0.0410 / 0.947 / 8.51 / 0.947 | 0.0420 / 0.691 / 6.77 / 0.732 | 0.0420 / 0.657 / 5.26 / 0.710 | 0.0450 / 0.619 / 3.35 / 0.619 |
| 2 | 0.0410 / 0.663 / 5.41 / 0.663 | 0.0410 / 0.710 / 5.23 / 0.762 | 0.0410 / 0.657 / 4.71 / 0.973 | 0.0450 / 0.648 / 3.43 / 0.648 |
| 4 | 0.0450 / 0.713 / 5.19 / 0.713 | 0.0440 / 0.629 / 3.73 / 0.629 | 0.0430 / 0.621 / 3.78 / 0.621 | 0.0440 / 0.657 / 2.61 / 0.657 |

**The other rules, on `burst40`** — the same figures, for the hold with its guard off and for the mean, at the window taken and at the widest measured:

| rule | behind | ahead | 12 elements: mean / worst / flips / arriving | 16 elements: mean / worst / flips / arriving |
|---|---|---|---|---|
| hold, guarded | 0 | 2 | 0.0490 / 0.916 / 4.85 / 0.700 | 0.0420 / 0.688 / 6.87 / 0.688 |
| hold, guarded | 2 | 2 | 0.0520 / 0.916 / 4.25 / 0.711 | 0.0410 / 0.657 / 4.71 / 0.973 |
| hold, guarded | 4 | 4 | 0.0570 / 0.918 / 2.94 / 0.653 | 0.0440 / 0.657 / 2.61 / 0.657 |
| hold, unguarded | 0 | 2 | 0.0490 / 0.919 / 4.77 / 0.700 | 0.0410 / 0.694 / 6.24 / 0.694 |
| hold, unguarded | 2 | 2 | 0.0530 / 0.919 / 4.25 / 0.711 | 0.0410 / 0.699 / 5.37 / 0.973 |
| hold, unguarded | 4 | 4 | 0.0560 / 0.915 / 2.39 / 0.652 | 0.0440 / 0.656 / 2.53 / 0.656 |
| mean | 0 | 2 | 0.0530 / 0.937 / 5.50 / 0.741 | 0.0410 / 0.937 / 7.09 / 0.690 |
| mean | 2 | 2 | 0.0520 / 0.937 / 5.02 / 0.720 | 0.0420 / 0.937 / 5.71 / 0.688 |
| mean | 4 | 4 | 0.0550 / 0.929 / 3.46 / 0.688 | 0.0460 / 0.937 / 3.39 / 0.613 |

**Ahead.** A second block ahead is free where nothing starts: `crowd40` is
unchanged to the digit at any reach, since its guard never fires on a steady
tone, and `broad40` moves by a thousandth either way. Where things do start it
pays: on `burst40` at twelve elements it takes the flips from 6.62 to 4.85 a
block, the travel from 4.23° to 3.59°, the jumps over fifteen degrees from 62
to 49, the cold restarts from 9 to 4, and the worst object on arrival from
0.949 to 0.700 — the object that appears no longer sets the worst case — for
a mean error that *falls*, 0.0510 to 0.0490. At sixteen it takes a sixth off
the flips for two per cent of the mean. A third and a fourth block ahead buy
more flips still, half of them at four, and start paying for it: four to five
per cent of the mean on `burst40`, two on `broad40` at twelve. The largest
reach that does not cost error is two, and that is what ships.

**Behind.** A block or more behind buys flips on every scene — a tenth on
`crowd40` at twelve, a third at sixteen, a third on `burst40` — and costs
error on the contested twelve-element folds: five per cent of the mean on
`crowd40`, five per cent of the worst object on `broad40`, six per cent of the
mean on `burst40` with two behind. At sixteen elements it gains on both counts.
That is a hold doing what a hold does — a sound that stops keeps its weight
for a block, and the ripple of a steady tone is held at its crest — and on a
fold where every element is contested the weight it invents is weight taken
from something else. Not taken, and recorded: it is the half of this lever
that a sixteen-element fold would want and a twelve-element one pays for.

**The other rules.** The hold with its guard off costs `crowd40` eighteen per
cent of the mean at twelve elements, as the anticipation's own table above
already said, and on `burst40` it is no steadier than the guarded one. The
mean is the wrong shape for an onset: it reaches a sound that appears at a
third of its weight, so the element arrives late, and the worst object on
`burst40` is a fifth worse than the hold's at every window. What a hold gives
an onset is the whole of its weight, a block early, and that is the quality
wanted.

**The two masters whose fold is the identity.** `prog11` and `cc` report
0.000 on every presentation, no flips and no travel, under every window
measured, as they must: with an element for every object there is nothing for
an energy to decide. `cc` cannot be encoded at all — it carries a bed the
encoder refuses, which is the fourth lever's gap — so the harness is its only
witness. `prog11` encodes, and its stream is **not** byte for byte what it
was: one payload more, 62 bytes on thirteen million. At a cut fifteen seconds
in, two objects arrive on the rear ceiling and two elements are re-purposed to
carry them; which of the two goes where is settled at the block the arrival
is first seen, and the look-ahead moves that block one earlier. Both answers
are exact and both move one element across the room; from there on the two
streams carry the same signals in two different channel numbers. That is the
criterion's letter not met, and its spirit — that a fold with nothing to
decide decides nothing differently — met: what changed is a tie, and a tie is
what a look-ahead is entitled to break a block sooner.

**What ships.** `hz_cluster::smooth::SMOOTHING`: a guarded hold, nothing
behind and two blocks ahead. The encoder holds three spans of source rather
than two, and a block costs the smoother a pass over the window and no
allocation.

## Objects that render differently never share an element

Everything above folds by where an object is and how loud it is, and nothing
by *how it is to be rendered*. A master states that too — whether an object
snaps to the nearest speaker, which zones of the room it may use, whether its
height counts, how far it follows the screen — and no master to hand ever
varies any of it, which is why nothing carried it. An authored master will.
Folding a voice snapped to the centre together with a free effect changes what
one of them means, and a quiet kind of content can end up with no element at
all in a loud scene.

**What was built.** `hz_render::Mode` is the tuple of those fields in the
codes the payload writes — snap, zones, elevation, screen — carried from the
master's own words (`hz_io::master`) through the description
(`RenderHints`) to the keyframes, the scene and the object. A mode is a
**class**, and `hz_cluster::class` makes the class a hard constraint of the
partition: an element belongs to one class, an object is only ever owned by
an element of its class, the fit only ever spreads it over elements of its
class, the exchange never takes a class's last element and re-homes what it
leaves behind, the hysteresis never crosses a class, the numbering charges a
change of class more than any angle, and the metadata written for an element
are its class's, exact rather than a mixture. The elements are shared between
the classes by the **marginal gain of the metric**: the seeding is incremental
— the loudest, then the worst served — so it yields in one pass what a class
would cost with one seed, two, three, judged on the rendered gain vectors of
the presentations; every class present gets one element, then the rest go
one at a time where the next gains most, and the share is held between blocks
unless a move pays by five per cent. The Lloyd passes and the placement
search then run once, on the share kept. Two elements of different classes
are never coincident, since a snapped voice and a free effect at the same
place are two elements at that place. The harness counts what the classes
are built to prevent anyway — an object reaching an element of another
class, a class present with no element — and folds with `--classes off` to
say what they cost.

**Scenes with classes.** `cargo xtask make-master --snap-every N` snaps every
Nth object, `--zone-every M` confines every Mth to the front, and each makes
a class: `class40` is `broad40` with every third object snapped, `class40z`
the same with every fourth snapped and every sixth confined, so three classes
and a fourth for the objects that are both; `crowd40s` and `burst40s` are
`crowd40` and `burst40` with every fifth and every third snapped.

**What it measured.** Classes off and on, the share held by five per cent,
by nothing, and by a hundred per cent, which never moves an element once
placed:

**`class40`**

| elements | classes | held by | mean / worst | arriving worst | crossings a block | unhoused a block | flips | travel | jumps | restarts | µs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 12 | off | — | 0.064 / 0.613 | 0.613 | 13.96 | 1.00 | 3.08 | 2.24° | 35 | 0 | 379 |
| 12 | on | 0.05 | 0.129 / 1.409 | 1.409 | 0.00 | 0.00 | 5.87 | 4.72° | 94 | 6 | 357 |
| 12 | on | 1.00 | 0.130 / 1.295 | 1.295 | 0.00 | 0.00 | 3.31 | 3.49° | 53 | 4 | 360 |
| 12 | on | 0.00 | 0.129 / 1.409 | 1.409 | 0.00 | 0.00 | 5.87 | 4.72° | 94 | 6 | 358 |
| 16 | off | — | 0.055 / 0.607 | 0.607 | 13.96 | 1.00 | 3.56 | 2.07° | 49 | 1 | 474 |
| 16 | on | 0.05 | 0.088 / 1.173 | 1.173 | 0.00 | 0.00 | 5.83 | 3.46° | 79 | 5 | 457 |
| 16 | on | 1.00 | 0.089 / 1.409 | 1.409 | 0.00 | 0.00 | 6.42 | 3.55° | 96 | 5 | 456 |
| 16 | on | 0.00 | 0.087 / 1.173 | 1.173 | 0.00 | 0.00 | 5.81 | 3.50° | 81 | 5 | 462 |

**`class40z`**

| elements | classes | held by | mean / worst | arriving worst | crossings a block | unhoused a block | flips | travel | jumps | restarts | µs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 12 | off | — | 0.064 / 0.613 | 0.613 | 12.96 | 3.00 | 3.08 | 2.24° | 35 | 0 | 371 |
| 12 | on | 0.05 | 0.170 / 1.564 | 1.564 | 0.00 | 0.00 | 2.95 | 5.13° | 96 | 6 | 342 |
| 12 | on | 1.00 | 0.169 / 1.564 | 1.564 | 0.00 | 0.00 | 2.55 | 4.57° | 80 | 6 | 343 |
| 12 | on | 0.00 | 0.170 / 1.564 | 1.564 | 0.00 | 0.00 | 3.04 | 5.25° | 99 | 6 | 344 |
| 16 | off | — | 0.055 / 0.607 | 0.607 | 12.96 | 3.00 | 3.56 | 2.07° | 49 | 1 | 474 |
| 16 | on | 0.05 | 0.101 / 1.567 | 1.567 | 0.00 | 0.00 | 5.72 | 4.39° | 118 | 9 | 435 |
| 16 | on | 1.00 | 0.101 / 1.567 | 1.567 | 0.00 | 0.00 | 4.99 | 3.86° | 101 | 8 | 433 |
| 16 | on | 0.00 | 0.101 / 1.567 | 1.567 | 0.00 | 0.00 | 5.68 | 4.40° | 118 | 8 | 436 |

**`crowd40s`**

| elements | classes | held by | mean / worst | arriving worst | crossings a block | unhoused a block | flips | travel | jumps | restarts | µs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 12 | off | — | 0.038 / 0.308 | 0.308 | 8.05 | 0.99 | 1.76 | 1.73° | 36 | 1 | 340 |
| 12 | on | 0.05 | 0.158 / 1.182 | 1.182 | 0.00 | 0.00 | 11.54 | 6.28° | 185 | 6 | 346 |
| 12 | on | 1.00 | 0.154 / 1.532 | 1.532 | 0.00 | 0.00 | 8.04 | 4.53° | 126 | 8 | 354 |
| 12 | on | 0.00 | 0.160 / 1.132 | 1.132 | 0.00 | 0.00 | 12.41 | 6.80° | 199 | 6 | 353 |
| 16 | off | — | 0.024 / 0.233 | 0.233 | 8.00 | 1.00 | 3.26 | 1.76° | 48 | 3 | 441 |
| 16 | on | 0.05 | 0.075 / 0.725 | 0.725 | 0.00 | 0.00 | 4.45 | 2.78° | 76 | 1 | 433 |
| 16 | on | 1.00 | 0.075 / 0.725 | 0.725 | 0.00 | 0.00 | 2.37 | 2.05° | 58 | 2 | 422 |
| 16 | on | 0.00 | 0.075 / 0.725 | 0.725 | 0.00 | 0.00 | 4.45 | 2.78° | 76 | 1 | 430 |

**`burst40s`**

| elements | classes | held by | mean / worst | arriving worst | crossings a block | unhoused a block | flips | travel | jumps | restarts | µs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 12 | off | — | 0.049 / 0.916 | 0.700 | 8.39 | 0.99 | 4.85 | 3.59° | 49 | 4 | 392 |
| 12 | on | 0.05 | 0.092 / 1.520 | 1.520 | 0.00 | 0.00 | 8.74 | 8.20° | 191 | 11 | 383 |
| 12 | on | 1.00 | 0.094 / 1.520 | 1.520 | 0.00 | 0.00 | 7.65 | 7.37° | 185 | 12 | 386 |
| 12 | on | 0.00 | 0.092 / 1.520 | 1.520 | 0.00 | 0.00 | 9.02 | 8.46° | 198 | 11 | 416 |
| 16 | off | — | 0.042 / 0.688 | 0.688 | 8.38 | 1.00 | 6.87 | 3.64° | 62 | 7 | 485 |
| 16 | on | 0.05 | 0.056 / 1.389 | 1.308 | 0.00 | 0.00 | 8.24 | 5.62° | 154 | 12 | 474 |
| 16 | on | 1.00 | 0.057 / 1.359 | 1.309 | 0.00 | 0.00 | 7.96 | 5.75° | 155 | 12 | 474 |
| 16 | on | 0.00 | 0.055 / 1.389 | 1.308 | 0.00 | 0.00 | 8.74 | 5.67° | 162 | 13 | 474 |

Zero crossings and no class without an element, by construction, and counted
anyway: the check is in the harness and not in the argument. The two masters
whose fold is the identity have one class and report 0.000 as before, and
`harlettizer encode` writes byte for byte the stream it wrote on `prog11`,
`crowd40` and `burst40`, which is what a master with nothing to keep apart
has to do; on `class40` it honours the classes, as it should, and writes a
different stream — 0.139 against 0.064 of fold, the same price the harness
reports.

**The price.** A hard constraint is paid for, and this one is paid twice.
The mean doubles to quadruples at twelve elements — 0.064 to 0.129 on
`class40`, 0.038 to 0.158 on `crowd40s` — and the worst object goes past one,
because each class is now folded into its own share of the elements: fourteen
snapped objects on a ring with six elements between them sit up to thirty
degrees from the nearest element of their class where they sat fifteen from
the nearest of anybody's, and an object that far from every element it may
reach is an object a fold cannot place. That is the cost of not merging what
must not be merged, and it is what an authored master with a snapped object
every third one would pay; at sixteen elements it is a third to a half. And
the fold is less steady: three to five times the jumps, twice the travel, and
cold restarts in one block of twenty to one of twelve where there were none,
because a class of six elements has more to gain from starting over than a
scene of twelve, and gains it more often. Holding the share at a hundred per
cent — no element ever changes class — takes a third off the jumps on the
turning scenes and nothing off the bursting one, where it is the restarts
that move things; five per cent is kept, because it is the only value that
ever lets a class that grew take an element, and a share held for ever is a
share decided on the first block.

**What is not done.** The `dialog` field a master carries per object is a
kind of content and not a rendering, and the payload has nowhere to state
it, so it is not a class here; a master that marks its dialogue would want
the second half of this lever — every kind of content present gets an
element — and the field is read and left alone. A description from elsewhere
carries `channelLock` and `screenRef` where a master carries snap and screen,
and `zoneExclusion` as a box geometry the master's named zones do not map
onto without a rendering decision; none of it is read yet, so every object of
such a file is one class. And more classes present than elements is refused
by name rather than folded wrongly, since a class cannot go without.

## A bed is folded as what it is

`harlettizer encode` refused any bed but the low frequency channel, and an
authored master is a 7.1.2 bed plus objects: the first real master this will
receive did not pass. A bed channel is a speaker feed — audio that belongs at
one speaker and nowhere else — and folded, that is an object that never moves
from its speaker's place in the cube.

**What was built.** A bed channel is carried by a folding encode as an
object with one keyframe, at the place `hz_render::fold::bed_position` gives
its label, in the **bed class** — `hz_cluster::class::BED`, an object that
snaps to the nearest speaker, which is what a speaker feed is and which the
snapped objects of a mix share. What the fold then does with it is one of
two things, `--beds`: **pinned**, an element of its own at that place, left
out of the fit entirely, which is the mechanism the harness always used; or
**free**, an ordinary object of the bed class, sharing the elements with the
rest by the metric's own rule, the third lever's. **Auto**, the default, pins
while every object can still have an element beside the bed and frees
otherwise, since a bed that takes every element leaves the objects nothing.
The low frequency channel is neither and keeps an element of its own. A label
with no place in the cube is refused rather than guessed at, and a bed other
than the LFE is still refused by an encode that does not fold, since a stream
of one element per object has no place to put a speaker feed but an object.
`cargo xtask make-master --bed 7.1.2` writes the bed an authored master has,
and `xtask cluster --beds pinned|free` measures either.

**A bed alone costs nothing.** The 7.1.2 bed of `bed712` — nine channels
and the LFE, no objects — into twelve and into sixteen elements: 0.000 mean,
0.000 worst, no travel and no flips in the harness, and `fold 0.0000` from
the encoder, pinned under the default. A scene of pinned objects and nothing
else used to be a scene the partition had never met, and it is a test now.

**A bed and forty objects.** `bed712_40` is the broadband scene over the
7.1.2 bed, the bed's channels carrying tones louder than any effect:

| | harness, 12 elements | harness, 16 elements | encoder, 12 elements | encoder, 16 elements |
|---|---|---|---|---|
| pinned | 0.234 / 0.232 | **0.070 / 0.069** | 0.348 / 1.539 | 0.093 / 0.892 |
| free | **0.187 / 0.219** | 0.079 / 0.078 | **0.223 / 1.424** | 0.093 / 0.892 |

Mean over the presentations and on the other ruler in the harness, mean and
worst object in the encoder, whose twelve elements are eleven and an LFE.
Where the bed has nearly every element — nine of eleven — pinning it starves
the forty objects, and freeing it is worth a fifth of the mean: the share by
marginal gain gives the bed fewer elements than it has channels and the
objects the rest. Where there is room, pinning wins by a tenth in the
harness, and in the encoder the two are the same fold to four figures: the
bed's channels are the loudest things in the scene, the share gives each of
them an element, and an object alone in an element of its own is a pin by
another name. Auto is free in both, since forty-nine objects do not fit in
fifteen elements, and it costs nothing where free and pinned agree.

**What is not done.** The rule for auto is a count and not a measurement:
the metric would say at sixteen elements that pinning is a tenth better in
the harness, and the fold does not ask it. The bed's channels here are tones,
which is not what a bed carries. And the master's bed is placed by the seven
labels the fold has a cube position for, one tier up for the heights; a
wide-front or a second LFE has none and is refused.

## Seeding by capture, measured and not taken

The seed is "the loudest, then the worst served": the loudest object first,
then repeatedly the object furthest by direction from any seed so far,
weighted by its energy. That is an energy-times-angle rule, and two things
can be said against it. It can open an element for an isolated quiet object
before a cluster of loud ones, since a quiet object far from every seed
scores as much as a loud one near one; and it does not know what an element
placed there would *capture* — an element serves everything whose rendered
gain vector overlaps its own, not only the object it sits on.

**What was built.** `hz_cluster::seed`: a candidate's score is the
importance an element at its position would serve that nothing serves yet,
the serving kernel being the overlap of rendered gain vectors over the four
presentations — the cosine between what the candidate radiates and what
each object radiates, which the fit already computes and which is the
metric's own language. No radius and no distance in the cube. Take the best
candidate, subtract from every object the share it now serves, repeat; the
greedy maximisation of a submodular coverage, within a constant of the best
any seeding could do. Every object is a candidate, so an element starts on
an object as it did. The rule seeds a cold block, the cold restart, and the
order a class's curve is built in; the Lloyd passes and the placement search
follow as before. `--seeding direction|capture` on the harness.

**What it measured.** On the ring of forty-eight, folded once from cold:

| elements | by direction | by capture |
|---|---|---|
| 6 | **0.161** / 0.870 | 0.186 / **0.789** |
| 8 | **0.087** / **0.510** | 0.115 / 0.708 |
| 12 | 0.048 / 0.339 | **0.047** / **0.230** |
| 16 | 0.033 / 0.212 | **0.030** / **0.191** |

And over the scenes, block by block, with everything else as it ships:

| scene | elements | seeded by | mean / worst | worst on arrival | flips | travel | jumps | restarts | exchanges | µs a block |
|---|---|---|---|---|---|---|---|---|---|---|
| `crowd40` | 6 | direction | 0.160 / 0.862 | 0.862 | 1.47 | 1.68° | 19 | 0 | 1 | 200 |
| `crowd40` | 6 | capture | **0.160 / 0.881** | 0.881 | 1.41 | 1.90° | 18 | 0 | 0 | 199 |
| `crowd40` | 8 | direction | 0.085 / 0.594 | 0.594 | 1.71 | 1.83° | 28 | 1 | 1 | 244 |
| `crowd40` | 8 | capture | **0.095 / 0.573** | 0.573 | 1.57 | 1.74° | 24 | 1 | 1 | 245 |
| `crowd40` | 12 | direction | 0.038 / 0.308 | 0.308 | 1.76 | 1.73° | 36 | 1 | 3 | 341 |
| `crowd40` | 12 | capture | **0.046 / 0.279** | 0.279 | 1.97 | 1.60° | 35 | 0 | 0 | 340 |
| `crowd40` | 16 | direction | 0.024 / 0.233 | 0.233 | 3.26 | 1.76° | 48 | 3 | 0 | 444 |
| `crowd40` | 16 | capture | **0.030 / 0.205** | 0.205 | 2.30 | 1.73° | 50 | 0 | 2 | 448 |
| `broad40` | 6 | direction | 0.143 / 0.892 | 0.892 | 2.14 | 2.39° | 23 | 2 | 0 | 218 |
| `broad40` | 6 | capture | **0.142 / 0.937** | 0.937 | 2.11 | 2.65° | 20 | 2 | 0 | 217 |
| `broad40` | 8 | direction | 0.096 / 0.649 | 0.649 | 1.79 | 2.38° | 24 | 0 | 0 | 267 |
| `broad40` | 8 | capture | **0.096 / 0.650** | 0.650 | 1.75 | 2.30° | 24 | 0 | 0 | 270 |
| `broad40` | 12 | direction | 0.064 / 0.613 | 0.613 | 3.08 | 2.24° | 35 | 0 | 1 | 374 |
| `broad40` | 12 | capture | **0.067 / 0.617** | 0.617 | 3.33 | 2.40° | 37 | 1 | 0 | 374 |
| `broad40` | 16 | direction | 0.055 / 0.607 | 0.607 | 3.56 | 2.07° | 49 | 1 | 0 | 481 |
| `broad40` | 16 | capture | **0.053 / 0.678** | 0.678 | 4.10 | 2.24° | 50 | 2 | 0 | 479 |
| `burst40` | 6 | direction | 0.116 / 1.257 | 1.126 | 3.25 | 4.73° | 45 | 8 | 1 | 221 |
| `burst40` | 6 | capture | **0.115 / 1.257** | 1.126 | 3.71 | 5.26° | 45 | 10 | 2 | 224 |
| `burst40` | 8 | direction | 0.079 / 0.931 | 0.879 | 3.71 | 4.28° | 41 | 6 | 2 | 279 |
| `burst40` | 8 | capture | **0.079 / 0.926** | 0.935 | 2.87 | 4.13° | 35 | 1 | 2 | 276 |
| `burst40` | 12 | direction | 0.049 / 0.916 | 0.700 | 4.85 | 3.59° | 49 | 4 | 3 | 380 |
| `burst40` | 12 | capture | **0.051 / 0.890** | 0.792 | 4.66 | 3.82° | 56 | 3 | 4 | 383 |
| `burst40` | 16 | direction | 0.042 / 0.688 | 0.688 | 6.87 | 3.64° | 62 | 7 | 2 | 478 |
| `burst40` | 16 | capture | **0.042 / 0.672** | 0.672 | 5.79 | 3.41° | 69 | 5 | 1 | 478 |
| `class40` | 6 | direction | 0.291 / 1.665 | 1.665 | 4.05 | 5.46° | 55 | 7 | 1 | 213 |
| `class40` | 6 | capture | **0.293 / 1.665** | 1.665 | 3.81 | 4.91° | 55 | 5 | 1 | 218 |
| `class40` | 8 | direction | 0.213 / 1.522 | 1.522 | 2.98 | 3.45° | 32 | 4 | 0 | 263 |
| `class40` | 8 | capture | **0.224 / 1.547** | 1.547 | 10.56 | 8.23° | 118 | 13 | 4 | 277 |
| `class40` | 12 | direction | 0.129 / 1.409 | 1.409 | 5.87 | 4.72° | 94 | 6 | 2 | 371 |
| `class40` | 12 | capture | **0.132 / 1.486** | 1.486 | 11.02 | 6.27° | 136 | 12 | 4 | 382 |
| `class40` | 16 | direction | 0.088 / 1.173 | 1.173 | 5.83 | 3.46° | 79 | 5 | 2 | 463 |
| `class40` | 16 | capture | **0.089 / 1.108** | 1.108 | 8.61 | 4.41° | 115 | 9 | 3 | 480 |

**Not taken.** On `crowd40` the capture takes a tenth off the worst object
and puts a fifth on the mean, at twelve and at sixteen; on `broad40` and
`burst40` the two are the same fold to a thousandth or two; on the ring it
wins at twelve and sixteen and loses at six and eight; and on `class40` it
doubles the flips and the jumps, since each class's seeds are then chosen by
what they capture within the class and the share between the classes turns
over with them. It costs nothing to run — the kernel is a few tens of
thousands of multiplications a cold block — and it buys nothing that holds
across the scenes. A seed that serves the most importance is not, it turns
out, the seed the passes that follow settle best from: what the Lloyd
passes want is a first guess in the right basins, and an angle-and-energy
rule finds those about as well as a coverage does, while the coverage
concentrates its seeds where the loud objects are and leaves the quiet
edges of the scene to be found by the passes, which do not always find
them. The directional rule stays, and the capture stays compiled, one
constant away — `hz_cluster::seed::SEEDING` — for the next scene that might
want it. The masters whose fold is the identity are unchanged, and so is
every stream.

## The dominant member as a candidate, measured and not taken

An element sits where the metric is smallest, never exactly on its loudest
member: a dominant voice can be a degree or two from its place. The obvious
rule — put the element on its loudest member — is a rule, and this project
does not take rules over measurements. So it is one more candidate instead:
the position of the member carrying the most of the element's energy,
offered to the coordinate descent beside the six steps and judged by the
metric like them, under `place::Search::dominant`. And a column beside it,
which is the more useful half: the harness now reports how far each element
carrying something sits from its dominant member, on average and at most,
whichever way the search is set.

**What it measured.** Everything else as it ships, the candidate off and on:

| scene | elements | dominant candidate | mean / worst | flips a block | dominant member from its element | µs a block |
|---|---|---|---|---|---|---|
| `crowd40` | 12 | off | 0.038 / 0.308 | 1.76 | 2.08°, 5.3° at most | 343 |
| | | on | 0.038 / 0.308 | 1.73 | 2.08°, 5.3° | 365 |
| `crowd40` | 16 | off | **0.024** / 0.233 | 3.26 | 2.09°, 4.9° | 438 |
| | | on | 0.027 / **0.193** | **2.01** | 1.75°, 4.9° | 466 |
| `broad40` | 12 | off | 0.064 / 0.613 | 3.08 | 4.16°, 13.5° | 377 |
| | | on | 0.064 / 0.615 | 3.03 | 4.14°, 13.6° | 400 |
| `broad40` | 16 | off | 0.055 / 0.607 | 3.56 | 2.99°, 15.2° | 474 |
| | | on | 0.055 / 0.607 | 3.38 | 3.06°, 17.3° | 510 |
| `burst40` | 12 | off | **0.049** / 0.916 | **4.85** | 5.72°, 119.6° | 375 |
| | | on | 0.051 / 0.916 | 5.49 | 5.79°, 102.3° | 405 |
| `burst40` | 16 | off | 0.042 / 0.688 | 6.87 | 7.12°, 141.2° | 473 |
| | | on | 0.041 / 0.688 | 7.39 | 7.01°, 134.4° | 507 |
| `class40` | 12 | off | 0.129 / 1.409 | 5.87 | 6.69°, 35.6° | 361 |
| | | on | 0.129 / 1.409 | 5.93 | 6.66°, 35.6° | 390 |
| `class40` | 16 | off | 0.088 / 1.173 | 5.83 | 5.40°, 36.7° | 461 |
| | | on | 0.087 / 1.173 | 5.89 | 5.35°, 36.7° | 492 |

**Not taken.** On six tables of eight the candidate changes nothing to a
thousandth, which is the search saying it was already there: two to seven
degrees is where a dominant member sits from its element under the metric,
and the metric put it there on purpose, since an element a degree or two
off its loudest member serves the members beside it better than one on top
of it. On `crowd40` at sixteen it takes a sixth off the worst object and a
third off the flips for a fifth on the mean; on `burst40` at twelve it puts
four per cent on the mean and a seventh on the flips. And it costs seven per
cent of the clustering's time, a panning and a score for every element every
pass. Off, and kept where the harness can turn it on. What stays is the
column: a dominant member sits two degrees from its element on a scene of
tones and four to seven on the broadband ones, and the hundred-and-forty
degrees at most on the bursting scene is an element carrying an object that
has just appeared across the room from where the element was, which is the
anticipation's price and not the placement's.

## The floor is where the room puts it

The floor under what an object has to carry to count was forty decibels
under the loudest object in the block, and nothing else. Being relative, it
promotes noise in a quiet block — the loudest thing in a block of nothing is
still the loudest — and hides a real object in a loud one, where a voice
forty decibels under an explosion is still a voice. What decides whether an
object can be heard is not its neighbour but the level it plays at.

**What was built.** `hz_cluster::floor`: an absolute floor in decibels
below full scale, derived from the playback level the programme's dialnorm
implies in three steps that are each somebody's standard. A decoder brings
the dialogue to −31 (ATSC A/52), so a programme at a dialnorm of `D` plays
`−31 − D` decibels quieter than the stream states it; a room plays pink
noise at −20 dBFS at 85 dB SPL (SMPTE RP 200), so full scale is 105 dB; and
the threshold of hearing in quiet at one kilohertz is 2.4 dB SPL (ISO 226).
So an object is inaudible below `2.4 − (105 + (−31 − D))` dBFS: −102.6 for a
programme at the reference, −91.6 for one at −20, −72.6 for one at −1.
Below it an object seeds nothing, sets no worst case and is not counted. It
still pulls the centre it is counted towards, so that an element carrying
only near-silence stays with its object rather than jumping to it when it
speaks — which was measured, since the first version pulled nothing and
took `prog11` from one jump to five. The relative floor stays as a second
guard, for the object that is audible in the room and not beside its
neighbour. `harlettizer encode --dialnorm` and `xtask cluster --floor`
state the dialnorm, or `--floor relative` for the floor as it was; the
report says where the floor fell and how many objects a block sat under it,
and the harness says how many of those the relative floor would have kept.

One threshold for every band, taken at one kilohertz, since the plain power
the fold weighs by has no bands. The perceptual rule's bands could each have
ISO 226's own threshold — that is the per-band floor the lever names — and
it is not written until the rule that would use it ships.

**What it measured.** At twelve elements, the encoder's block:

| | under the relative floor a block | under the room's floor a block, at −31 | at −20 | at −1 | the room's floor takes that the relative kept, at −31 / −1 |
|---|---|---|---|---|---|
| `prog11` | 3.61 of 11 | 3.33 | 3.33 | 3.88 | 0.00 / 0.28 |
| `cc` | 15.00 of 15 | 15.00 | 15.00 | 15.00 | 0.00 / 0.00 |
| `crowd40` | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 / 0.00 |
| `broad40` | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 / 0.00 |
| `burst40` | 16.19 | 16.19 | 16.19 | 16.19 | 0.00 / 0.00 |

Close to zero, as expected, and the shape of it is worth reading. On
`prog11` a third of the objects sit under −102.6 dBFS at any moment — the
channels of objects that are inactive carry a noise floor a hundred and
some decibels down — and every one of them was already under the relative
floor: the room's floor takes nothing the relative kept until the dialnorm
is −1, where it takes a quarter of an object a block. `cc`'s fifteen objects
are digital silence throughout, since its programme is in its bed, and are
under any floor at all. On the bursting scene the effects that are off are
under it and were under the relative one too. And on `crowd40` and `broad40`
nothing is under either, and nothing changes: the same error, flips and
travel to the digit, and the encoded stream of `crowd40` byte for byte.

**What changes.** `prog11`'s stream is not byte for byte what it was: the
same fold at 0.000 and the same 285 payloads, and 116 bytes more, because
the objects under the room's floor no longer seed an element at a cold
start and the elements start from somewhere else. Its travel and its jumps
are what they were. That is the floor doing the only thing it was built to
do on a master with no real object under it.

## No sample outside the domain

An element is a sum, and two coherent objects in the same element exceed
full scale. The encoder clamped and counted: 10 980 samples of `crowd40`
went outside the codec's domain over four seconds, since forty tones at
harmonically related frequencies line up now and then, and a scene built to
clip — `make-master --coherent 2`, the first two objects carrying one and
the same tone at −4 dBFS, nine degrees apart — put 16 708 outside. A clamp
is a crack.

**What was built.** Two things, which are not the same thing.

*A bound in the fit.* `hz_cluster::headroom`: what element `k` can reach
is bounded by `Σ_i w_ik · g_i · peak_i`, every object's block peak — which
the scene now measures — gained and weighted, adding coherently. When that
bound exceeds the domain, the objects the element carries are solved again
with an upper bound on their weight for it, each bound its share of the
headroom, so that the coherent worst case of the element is full scale and
no more. The bounded solve pins what the free answer puts past its bound at
the bound, takes what the pinned elements radiate off the target, solves
the rest for what is left, and puts the level back as far as the bounds
allow — the one thing the full method of Stark and Parker would reconsider,
a weight pinned at its bound coming back down, it does not, and with a
bound on one or two of sixteen elements no difference has been measured.
An object bounded keeps the set it had and the class it is in. What the
fit cannot put back it leaves off, and the metric's level column says so.

*A limiter on the residual.* `hz_cluster::mix::Limiter`: one gain for
every element, since a gain per element would move the image of an object
spread over several and the metric would not see it move. Offline, the
whole block is in hand before any of it is written, so the gain reaches
its lowest at the peak and comes down over two milliseconds before it,
rather than reacting after; it releases over fifty. The gain at the end of
a block carries into the next, so the join is continuous.

`harlettizer encode --headroom limit|bound|both|off`, and the report says
how many blocks were bounded, how many were limited, and what the mix
peaked at before the limiter; `xtask cluster --headroom on|off` measures
the bound alone, since the harness mixes no audio, and its level column
is where the bound's price shows.

**What it measured.** In the encoder, twelve elements:

| scene | headroom | fold, mean / worst | blocks bounded | blocks limited | peak before the limiter | samples clipped |
|---|---|---|---|---|---|---|
| `coherent40` | off | **0.054** / **0.611** | 0 | — | — | 16 708 |
| | bound | 0.099 / 0.681 | 150 of 150 | — | — | 2 398 |
| | limit | **0.054** / **0.611** | 0 | 64 | 1.79 | **0** |
| | both | 0.099 / 0.681 | 150 | 70 | 1.52 | **0** |
| `crowd40` | off | **0.053** / **0.332** | 0 | — | — | 10 980 |
| | bound | 0.063 / 0.565 | 150 of 150 | — | — | 4 609 |
| | limit | **0.053** / **0.332** | 0 | 150 | 1.33 | **0** |
| | both | 0.063 / 0.565 | 150 | 145 | 1.24 | **0** |

And in the harness, whose twelve elements are twelve and whose level column
sees the bound: on `coherent40` the bound takes the mean from 0.048 to
0.077 and the level from 1.000 to between 0.86 and 1.00; on `crowd40` it
takes nothing off the mean and a per cent off the level.

Under the default, the limiter alone, every scene encodes with no sample
outside the domain: `broad40` is limited in 53 blocks of 150 with the mix
peaking at 1.47 before it, `burst40` in 25 at 1.35, and `prog11`, which
peaks at 0.22, in none — its stream is byte for byte what it was. And the
limiter has a cost the metric does not see and the stream does: a gain
that moves is harder to code losslessly than a clamp, and `crowd40`'s
stream grows by seven per cent, 2 760 378 to 2 961 518 bytes, for its 150
limited blocks; `burst40`'s, limited in 25, shrinks by a thousandth.

**What ships is the limiter.** The bound is on the coherent worst case,
and forty tones in nine contributions an element pass that in every block
while the mix itself passes full scale in a few samples of some: on
`crowd40` the bound fires in every block, costs a fifth of the fold's mean
and two thirds of its worst object in the encoder, and halves the clips
rather than ending them — the weights ramp in from a block that was bounded
for other peaks, and an object's share moved off a bounded element pushes
the next one over past the three passes. The limiter takes every clip on
both scenes for nothing the metric sees. What it costs is a gain that dips
at the peaks, and that the report states rather than the metric: on
`crowd40` the mix reaches 1.33 of full scale in the loudest of its blocks,
so the gain there is 0.75 for two milliseconds and back within fifty, in
every block of that scene. `Headroom::Limit` is the default, the bound is
compiled behind `--headroom bound|both` and `hz_cluster::headroom::BOUNDING`,
and the objects' peaks are in the scene for it. The masters whose fold is
the identity never pass full scale and encode byte for byte.

**What is not done.** A bound that knew the phases — an incoherent
estimate, or the measured peak of the mix a block earlier — would fire where
the coherent one does not have to and might earn its place; this one does
not. And the limiter's look-ahead is the block's: a peak in the first two
milliseconds of a block is reached with what ramp there is.

## A bench for the listening that settles a tie

Every figure in this notebook is measured, not judged by ear, and the
notebook records ties the metric cannot settle: the plain power against the
K-weighted one and against the perceptual importance, the fitted weighting
against the nearest on widths. Listening settles a tie, and listening has to
be reproducible or it is an opinion.

**What was built.** `cargo xtask bench`: the scene and each rule's fold of
it, rendered on 7.1.4 with the repository's own panner — the vector-base
amplitude panning of Pulkki that the metric's fourth presentation already
is — from the same master over the same seconds, as blinded stimuli for a
session in the form of BS.1534. The **reference** is every object panned at
its own position, ramped across each block the way a decoder ramps a
payload, the bed at its speakers and the LFE passed through; the **anchor**
is the reference through a low-pass at 3.5 kHz, which is what the
recommendation puts at the bottom of the scale so that the scale has one;
and each **condition**, `loudness/weighting`, is the scene folded into the
elements asked for under that rule with everything else as it ships, the
elements mixed the way the encoder mixes them and panned where the fold put
them. Every stimulus is scaled by one gain, so that nothing passes full
scale and every level stays what it was against the others. The files are
named by a seeded shuffle with the reference hidden among them; `key.txt`
says which is which, for the scorer and not the listener, and `sheet.csv`
takes one score from nought to a hundred per file. `--score` joins a
filled-in sheet to the key, prints the ranking, and says when the hidden
reference scored under ninety, which is the listener BS.1534 leaves out.

```text
cargo xtask bench programme.atmos --out bench/ --seconds 8 \
    --condition flat/fitted --condition kweighted/fitted \
    --condition perceptual/fitted --condition flat/nearest
cargo xtask bench programme.atmos --out bench/ --score bench/sheet.csv
```

**What it has not done.** No sheet has been scored yet. The bench renders
`broad40` in a second and a half, six stimuli of four seconds each, and the
ranking it would print is the result this lever is for: the one that fixes
the metric's weighting, once, with the notebook saying on which scenes.
Until a sheet is in, every verdict above that rests on a tie — the
perceptual importance not taken, the smoothing's reach behind not taken —
rests on the metric alone, which cannot judge by ear, and says so. The
scenes to sit it on are the
ones where the metric's verdicts diverge: `crowd40` and `broad40` for the
loudness rules, a ring with widths for the weightings, and `burst40` for
the smoothing.

## What is not claimed

**That this matches the reference engine.** It cannot be checked here, and the
reason is not the reference engine — it is the *input*. Every master to hand
is the **output** of a clustering: counted, they carry eleven, thirteen or
fifteen objects, which is twelve, fourteen or sixteen elements. There is
nothing with more objects than a bitstream carries, so there is nothing to hand
two clusterers and compare. A reference encoder would have the same nothing to
cluster.

What would unblock it is authored content — a mix with sixty objects in it, from
a renderer rather than from a decoder. Until then the numbers here are what this
can honestly say: measured, reproducible, and about scenes whose ground truth is
known because they were made.

**That the error figures are good.** 0.13 to 0.24 means that much of an object's
gain vector lands somewhere else. Whether that is audible is a listening
question, which the bench above is for and the figure cannot answer. What the
figure is for is comparison: it falls with more elements, it distinguishes the
two weightings, and it will say whether the next idea is better than this one.

```bash
cargo xtask cluster programme.atmos --elements 12
```

## An element keeps its number

Everything above measures *where* the elements go. This is about which of them
is which, and it turned out to be most of the instability the reports were
showing.

Nothing in the clustering decided which slot a direction landed in. The seeding
takes objects in energy order; the assignment loop takes them in index order.
Both turn over as levels change, so a scene that has not moved comes back with
the same set of positions in a different order — over and over. On `prog11` at
**twelve** elements, where eleven objects go into eleven and the fold is exact,
the eleven positions were identical in every block and their order changed in
**348 blocks of 1129**.

An element is a channel, so that is not cosmetic. Moving a signal from channel
7 to channel 8 while moving channel 7's position to where channel 8's was is a
swap only if both happen at the same instant, and they do not: the payload ramps
the positions across the block and [`mix`](#) cross-fades the weights across it,
so a listener gets two signals sweeping past each other. Downstream, in
`harlettizer encode --cluster`, every relabelling also wrote a payload, which is
where a +22 % stream came from.

A relabelling is free — no position, no weight and no object's share changes —
so the one to pick is the one that keeps each element carrying what it carried.
That is an assignment problem, solved exactly by the Hungarian algorithm; at
sixteen elements it is a few thousand operations once a metadata block, and it
does not show up in the timing.

**Matched on what the element carries, not on where it points.** Direction
alone looks right and is not: two objects in the same place give two elements at
the same direction, the assignment between them costs the same either way, and
the two signals go on trading channels for as long as the objects stay
coincident. Measured — matching on direction still left 348 blocks writing a
payload and the folded stream at +3.9 %. The cost is the cosine between an
element's column of the weight matrix before and after, with the angle kept as
a thousandth of a term, which decides nothing except between elements carrying
nothing at all: those have no column to compare.

### What it is worth

Before and after, on `prog11` at twelve elements, 25 blocks a second:

| | travel a block | jumps over 15° | flips a block |
|---|---|---|---|
| before | 0.68° | 98 | 2.48 |
| after | **0.02°** | **3** | **0.04** |

And across the masters, as they stand now:

| master | elements | error mean / worst | travel a block | jumps | flips a block |
|---|---|---|---|---|---|
| prog11 (11 objects) | 12 | 0.000 / 0.000 | 0.02° | 3 | 0.02 |
| prog11 | 16 | 0.000 / 0.000 | 0.02° | 3 | 0.02 |
| cc (2 directions) | 12 | 0.000 / 0.000 | — | 0 | 0.00 |
| cc | 16 | 0.000 / 0.000 | — | 0 | 0.00 |
| crowd40 (40 objects) | 12 | 0.044 / 0.377 | 2.51° | 36 | 4.74 |
| crowd40 | 16 | 0.035 / 0.293 | 2.51° | 48 | 5.40 |

`prog11` and `cc` now give the same answer at twelve and at sixteen, which is
what a scene of eleven barely-moving objects and a scene of two directions
should give. `crowd40` still moves, and should: forty objects sharing twelve
elements have to change hands, and 2.5° a block is the scene turning rather
than the numbering churning.

### The weights had no memory, and now the set does

The hysteresis holds an object's *element* and the assignment holds an
element's *number*. The **weights** were solved from cold every block, so the
fit was free to answer a different question every 26.7 ms: for an object
between three near-equidistant elements the residual barely distinguishes
`{A,B}` from `{A,C}`, and it traded them back and forth. `mix` cross-fades the
amplitude, so it is not a click; the signal still moves from one channel to
another twenty-five times a second for no reason the scene gave.

A flip is an element going from **something to nothing**, which is a property
of the fit's *active set* and not of the weights in it. So the set is what is
made sticky: when there is a previous answer and the free fit does not land on
it anyway, the fit is solved a second time confined to the elements the last
block used, and the confined answer is taken if it costs no more than half
again the free one's residual.

| | error mean / worst | flips a block | paths |
|---|---|---|---|
| crowd40, 12 elements, before | 0.044 / 0.377 | 4.74 | 2.22 |
| crowd40, 12 elements, after | 0.044 / 0.377 | **2.79** | 2.19 |
| crowd40, 16 elements, before | 0.035 / 0.293 | 5.40 | 2.29 |
| crowd40, 16 elements, after | 0.035 / 0.293 | **2.23** | 2.26 |

The presentations' means are unmoved to five decimals — 0.01926 / 0.03456 /
0.04795 / 0.07426 either way at twelve elements. The two masters whose fold is
the identity are unchanged, as they must be: with an element per object there
is no second set to choose from. The second solve is skipped whenever the free
fit already lands on the held set, which is most blocks, and the clustering
costs 66 µs a block against 64.

Half again is the largest tolerance that costs nothing and the smallest that
gets the whole benefit: one buys nothing more, and two starts paying — 4 % of
the mean, which is the set being held past the point where the scene has left
it.

### The obvious way to do it makes it worse

The textbook way to hold a least-squares answer near a previous one is a
Tikhonov pull towards it, `‖A w − b‖² + λ‖w − w_prev‖²`. That is still a
non-negative problem and still the same solver. It was written first, and it
makes the flips **worse**:

| λ | mean error | worst | flips a block | paths |
|---|---|---|---|---|
| 0 | 0.044 | 0.377 | **4.74** | 2.22 |
| 0.01 | 0.044 | 0.377 | 5.40 | 2.23 |
| 0.03 | 0.044 | 0.377 | 6.16 | 2.24 |
| 0.1 | 0.045 | 0.378 | 7.50 | 2.28 |
| 0.3 | 0.047 | 0.396 | 8.27 | 2.32 |
| 1.0 | 0.057 | 0.646 | 8.03 | 2.47 |
| 3.0 | 0.089 | 0.879 | 9.37 | 2.67 |

The reason is in the gradient. A pull towards `w_prev` adds `λ w_prev[k]` to
the term that decides which element to free next, so it frees elements the
audio did not ask for merely because the last block used them — and they come
back with a weight near zero, which dies at the next block that does not want
it either. It manufactures the event it was meant to suppress, and the paths
column shows it: 2.22 elements an object at λ = 0, 2.67 at λ = 3.

A penalty on the weights is the wrong instrument for a property of the set,
however well it is tuned.

### The flip count now has a floor

The travel count ignored elements carrying nothing and the flip count did not,
so a near-silent object trading places with another near-silent one was reported
beside the ones a listener would hear. Both now use the same population — an
object within forty decibels of the loudest in the block, on an element that
carries something — which is the same floor `metric` applies to `worst`.
