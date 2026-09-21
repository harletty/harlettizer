# Clustering: what is still missing, and where to find it

September 2026. [`docs/clustering.md`](clustering.md) is the notebook of what
[`hz-cluster`](../crates/hz-cluster) does and what it measured. This is the list
of what it does not do yet: the qualities a listener would notice and that the
metric, as it stands, cannot see. Every lever below names a public source at the
end of this document, and that source is what [`docs/provenance.md`](provenance.md)
records the day the lever lands. The document is self-contained: an
implementation needs this file and the repository, nothing else.

## Rules of the game

1. **No constant comes from anywhere but a measurement.** A threshold, a time
   constant, a number of bands: each is chosen by `cargo xtask cluster` and
   written into `docs/clustering.md` with the alternatives that were rejected and
   their figures, the way the repository already does.
2. **The ruler changes with the rule.** As soon as a lever changes what an
   object weighs, the metric has to weigh the same way. Measure on both rulers,
   the old and the new, to separate what the rule gains from what the ruler
   moves. The principle is already written in `hz_cluster::scene`; it becomes
   mandatory.
3. **`crowd40` is a poor perceptual judge.** Forty pure tones over five octaves:
   any frequency weighting moves their ranking by 17 dB where broadband content
   would move it by two or three. Before any perceptual decision, build a
   broadband scene: a voice simulated as filtered noise, a rumble, wide effects.
   The two scenes decide together, never one alone.
4. **Measured, then listened to.** The figures in this repository are measured,
   not judged by ear. Where two rules tie on the metric, listening decides, and
   lever 9 gives that listening a reproducible form.
5. **What was measured winning is not replayed.** See the closing list.

## 1. A perceptual importance, and the same one in the metric

**What is missing.** An object's energy is a plain mean square, a meter's answer.
A rumble at −10 dBFS outweighs a voice at −20 dBFS ten to one, takes the seed,
pulls the centre and sets the floor under the worst case. K-weighting was tried
and doubled the error on `crowd40`, but see rule 3. And the worse defect is
elsewhere: the metric weighs with the same flat energy, so a perceptual mistake
in the weight is invisible to the judge.

**The quality wanted.** An object's weight is what its disappearance would
change in what is heard, not its power. That is a partial loudness with masking,
by frequency band.

**Where to look.** Specific loudness and its compressive law are in ISO 532-1 and
532-2 [1, 2]. The partial loudness of a sound in a noise is the model of Moore,
Glasberg and Baer [3]. ERB-wide bands come from Glasberg and Moore [4]. The
threshold of hearing in quiet is in ISO 226 [5]. The excitation model of PEAQ [6]
is another complete public reference. Clustering sound sources under a
perceptual criterion with masking is published by Tsingos, Gallo and
Drettakis [7].

**How to do it our way.**

- A band analysis of every object at every block, from the block's audio. The
  K filter of `hz_analysis` already exists; it gets a split into ERB-spaced
  bands [4]. The number of bands is chosen by measurement, not by tradition.
- An object's importance is the loudness of the mix with it minus the loudness
  without it, computed with the standard's compressive law [2], not with a
  shortcut.
- **The part that is ours: masking is spatial.** A distant masker hides less than
  one at the same place, which is the spatial release from masking Bronkhorst
  reviews [8]. So the masker of object `i` is the sum of the objects **near it
  in direction**, weighted by the overlap of their rendered gain vectors, which
  is already the metric's own language. Two variants to measure: a global
  masker, a local one.
- The metric weighs `e_i` by that importance. Report on both rulers, rule 2.

**How to judge it.** `crowd40` and the broadband scene of rule 3. The ceiling
object 38 dB under the loudest, the one that was folded to the floor, has to
keep its element if it is unmasked. Error, worst case, flips. An explicit
compute budget: a band analysis of every object at every block has to stay
within a few per cent of the encoder, measured over two hours the way
`docs/clustering.md` already does.

**Built and measured, not taken** (September 2026). `hz_analysis::bands`,
`hz_analysis::partial`, `Loudness::Perceptual` in `hz_cluster::scene` with a
global and a local masker, the harness judging every fold on both rulers, and
the broadband scene (`make-master --scene broadband`). The tables are in
[`docs/clustering.md`](clustering.md#what-its-disappearance-would-change-a-perceptual-importance-measured).
On the plain ruler no configuration beats the plain power on any of the four
tables; on its own ruler the rule gains on `crowd40` at sixteen elements and
loses on `broad40` at twelve; the analysis costs thirty per cent of the
encoder as written, with two known halvings not made. The plain power stays
the default and the rule stays one constant away, and `harlettizer encode
--loudness perceptual` folds a programme by it for the listening. What remains of this lever:
the listening of lever 9, which is what a tie on the metric leaves; the
anticipation's threshold restated in the new unit before any re-measurement;
and the two halvings of the cost, if a re-measurement ever wants them. The
ceiling-object check is `masking_is_by_band` in `hz_cluster::scene`; the
band analysis lever 7 needs is in the tree.

## 2. Smoothing the importance in time, offline

**What is missing.** A block's energy ripples with where the block boundary
falls in the waveform, as the notebook records. The ×2 anticipation handles
onsets only. The flips come from a fit answering a different question every
26.7 ms because the weights it is handed tremble.

**The quality wanted.** An importance that rises at the attack and does not fall
at the first dip, so that the decisions do not follow the signal's tremor.

**Where to look.** The integration windows of BS.1770 and EBU Tech 3341 [9, 10]
are standardised loudness smoothings. Envelope followers and their constants are
in Zölzer [11] and in the tutorial of Giannoulis, Massberg and Reiss [12].

**How to do it our way.** A real-time encoder smooths causally, with an attack
and a release. This encoder is offline and already reads a span ahead. So a
**non-causal** smoothing, centred on the block: a hold over a short window
around the block rather than a decay after it. The geometry stays aligned with
the audio, unsmoothed: leading the geometry was measured at twice the error.
The window width is chosen from the flips-against-error curve, the usual method.

**How to judge it.** Flips, travel, error on arrival and on departure of the
block. `prog11` and `cc` have to stay byte for byte identical.

**Built and measured, taken in part** (September 2026). `hz_cluster::smooth`:
a window of blocks around the one being placed, read as a guarded hold or as a
mean, with the anticipation as its one-block case; the encoder queues as many
spans as the window reaches ahead; the harness measures any window and
reports the error as each block arrives; and a third scene, `make-master
--scene bursts`, whose effects start and stop. The tables are in
[`docs/clustering.md`](clustering.md#the-energies-are-steadied-over-a-window-and-the-window-is-two-blocks-ahead).
What ships is the hold two blocks ahead and none behind: free where nothing
starts, and on the bursts a quarter off the flips and the object that
appears no longer the worst on arrival. The blocks behind buy flips on every
scene and cost error on the contested twelve-element folds, and are not
taken; the mean is the wrong shape for an onset. `prog11` is not byte for
byte what it was — one payload more at a cut, where a tie between two exact
answers is broken a block sooner — and `cc` cannot be encoded until lever 4.

**Reopened** (September 2026). The reach behind was refused on the
synthesised scenes, where it costs about five per cent of the mean on a
contested twelve-element fold. On a *programme* that cost is not there:
folded one element short of its sources, two blocks behind take 29 % off the
out-and-back excursions of the rear element that has them and 16 % off the
elements' travel, for a mean error that moves in the fifth decimal, and four
blocks take more travel still, and on the count of out-and-backs the
churn looked as though it moved onto another rear element rather than
ending — which the ruler of lever 10 has since contradicted, that count
being the wrong instrument: on the ruler the wobble falls monotonically,
3.63 % of the element-windows at nought behind, 2.20 % at two and 1.39 %
at four. So it is a knob and not a constant until a listening settles it:
`harlettizer encode --smooth-behind`, nought by default, and the
encoder's summary states what it steadied by. What it does not settle is
in lever 10.

## 3. Metadata classes

**What is missing.** Nothing carries them. No master to hand ever varies snap,
zones, size or render mode, but an authored master will. Folding a
snap-to-centre voice together with a free effect changes what the voice means,
and a quiet type of content can end up with no element at all in a loud scene.

**The quality wanted.** Two objects whose metadata render differently never
share an element, and every type of content present gets at least one element.

**Where to look.** The fields that change rendering are in BS.2076 [13]:
`zoneExclusion`, `screenRef`, `objectDivergence`, `channelLock`, `importance`,
and the `dialogue` attribute of `audioContent`, which separates dialogue,
non-dialogue and mixed. BS.2127 [14] says how the reference renderer honours
them. Dolby's public ADM profile [15] says what a master carries of them; check
that it is the freely downloadable edition, `LEGAL.md §3`. `hz-io` already reads
the master's equivalent fields.

**How to do it our way.**

- A **class key** per object: the tuple of the fields that change rendering.
- The class is a **hard constraint of the partition**: an element belongs to one
  class, and the ownership hysteresis never crosses a class.
- The element budget is shared between classes by **marginal gain of the
  metric**, not by a distance proxy. The seed is incremental, the loudest then
  the worst served, so it yields the curve "k elements for this class" in one
  pass. At least one element per class present, then one at a time to the class
  where it pays most, then a single Lloyd pass and a single placement search on
  the allocation kept.
- The Hungarian numbering gets a tiered cost: changing class costs more than any
  angle.
- The metadata written for an element are its class's, exact rather than a
  mixture.

**How to judge it.** A synthesised scene with two classes. Cross-class merges:
zero by construction, and counted anyway, since an indicator a rule is built to
satisfy is not a check on that rule. The error against the version without
classes, to know the price. `prog11` and `cc` unchanged.

**Built and measured, taken** (September 2026). `hz_render::Mode` carried
from the master to the object, `hz_cluster::class` sharing the elements by
the marginal gain of the metric and holding the share between blocks, the
class a hard constraint on ownership, fit, exchange, hysteresis and
numbering, the element's metadata its class's, and scenes with two to four
classes from `make-master --snap-every --zone-every`. The tables are in
[`docs/clustering.md`](clustering.md#objects-that-render-differently-never-share-an-element).
Zero crossings and no class without an element, counted; the price is a
doubling to a quadrupling of the mean at twelve elements and a less steady
fold, since each class is folded into its own share; `prog11` and `cc` are
unchanged and encode byte for byte. What remains: the `dialog` field as a
kind of content, and a description from elsewhere's `channelLock`,
`screenRef` and `zoneExclusion`, none of which is read into a mode yet.

## 4. Beds

**What is missing.** `harlettizer encode` refuses any bed but the LFE. An
authored master is a 7.1.2 bed plus objects. It is the first real master this
will receive, and it does not pass.

**The quality wanted.** A bed channel is folded as what it is: a motionless
object at the canonical direction of its speaker, or an element of its own when
the budget allows.

**Where to look.** The layouts are in BS.2051 [16], the common channel
definitions in BS.2094 [17], the `DirectSpeakers` type in BS.2076 [13].
`hz_core::speakers` already has the directions.

**How to do it our way.** A bed channel becomes an object **pinned** to its
canonical direction while an element is left for it, the existing `pinned`
mechanism, and otherwise an ordinary object at that direction with the "bed"
class of lever 3. The LFE keeps its element. The metric decides, as for
everything else.

**How to judge it.** A 7.1.2 bed alone into twelve elements has to cost zero.
Bed plus forty objects: pinned against free, over the four presentations.

**Built and measured, taken** (September 2026). A folding encode carries a
bed channel as an object at its speaker's place in the bed class — a mode
that snaps — pinned or free under `--beds auto|pinned|free`, auto pinning
while every object can still have an element beside the bed; the harness
measures either; `make-master --bed 7.1.2` writes the bed. The tables are in
[`docs/clustering.md`](clustering.md#a-bed-is-folded-as-what-it-is). The bed
alone costs 0.000 into twelve and sixteen; with forty objects, free is a fifth
better where the bed would take nearly every element and pinned a tenth
better where there is room, and in the encoder at sixteen the two are the
same fold, since the share gives every loud bed channel an element of its
own. What remains: auto is a count and not a measurement, and the labels
without a place in the cube are refused.

## 5. Seeding by capture, with a rendered-gain kernel

**What is missing.** The seed is "the loudest, then the worst served". That is
an energy-times-angle rule, which can open an element for an isolated quiet
object before a cluster of loud ones, and which does not know what an element
placed there would **capture**.

**The quality wanted.** Each new element goes where it serves the most
importance not yet served.

**Where to look.** k-means++ [18] for probabilistic seeding by distance, greedy
maximisation of submodular functions [19] for the guarantee on coverage, and
Tsingos [7] for the version driven by importance.

**How to do it our way.** A candidate's score is the importance an element at
its position would serve, and the "serving" kernel is the **overlap of rendered
gain vectors** over the presentations, already computed for the fit. No radius,
no distance in the cube. Take the best, subtract the share served, repeat. Lloyd
and the placement search follow as today, and the cold restart uses the same
seed.

**How to judge it.** The usual tables at 6, 8, 12 and 16 elements, on the ring
and on `crowd40`. The restart count. The cost a block.

**Built and measured, not taken** (September 2026). `hz_cluster::seed`:
the capture kernel over the rendered gain vectors, seeding cold blocks, cold
restarts and the classes' curves, `--seeding direction|capture` on the
harness. The tables are in
[`docs/clustering.md`](clustering.md#seeding-by-capture-measured-and-not-taken).
It wins on the ring at twelve and sixteen and loses at six and eight, takes
a tenth off the worst object of `crowd40` and puts a fifth on its mean,
ties on the broadband scenes, and doubles the flips of a scene with classes,
at no cost a block. The directional rule stays and the capture stays one
constant away.

## 6. The dominant object as a placement candidate

**What is missing.** An element sits where the metric is smallest, never exactly
on its loudest member. A dominant voice can be a degree or two from its place.

**The quality wanted.** When one member dominates an element, the element is at
that member's place.

**Where to look.** The k-medoid of Kaufman and Rousseeuw [20], where a centre has
to be a member.

**How to do it our way.** Not a rule: **one more candidate** in the coordinate
descent, the position of the dominant member, judged by the metric like the six
others. And a report column: distance from the dominant member to its element.

**How to judge it.** That column, the error tables, and the cost, one candidate
a pass.

**Built and measured, not taken** (September 2026). `place::Search::dominant`
offers the dominant member's position as a seventh candidate, and the
harness reports how far a dominant member sits from its element either way.
The tables are in
[`docs/clustering.md`](clustering.md#the-dominant-member-as-a-candidate-measured-and-not-taken):
nothing to a thousandth on six tables of eight, a fifth on the mean of
`crowd40` at sixteen for a sixth off its worst, four per cent on `burst40` at
twelve, and seven per cent of the clustering's time. The column stays: two to
seven degrees on average, which is where the metric puts an element on
purpose.

## 7. An absolute audibility floor

**What is missing.** The floor is 40 dB under the loudest object in the block.
Being relative, it promotes noise in a quiet block and hides a real object in a
loud one.

**The quality wanted.** An object is ignored because it is inaudible at the
playback level, not because a neighbour is loud.

**Where to look.** The playback level is known: `dialnorm` per A/52 [21],
measured by `hz_analysis` through BS.1770 [9], and the room calibration of
SMPTE RP 200 [22] or EBU R 128 [23] turns it into a sound pressure. The
threshold in quiet at that level is ISO 226 [5].

**How to do it our way.** An absolute threshold in dBFS, per band once lever 1
is in, derived from the playback level the programme's dialnorm implies. Below
it an object does not seed, does not set a worst case and is not counted. The
relative floor stays as a second guard.

**How to judge it.** Over the masters to hand, count the objects under the
floor: close to zero is expected. On `crowd40`, nothing may change.

**Built and measured, taken** (September 2026). `hz_cluster::floor`: the
threshold of hearing at the playback level the dialnorm implies — −102.6 dBFS
at the reference — under which an object seeds nothing, sets no worst case
and is not counted, with the relative floor as a second guard;
`encode --dialnorm` and `xtask cluster --floor`. The tables are in
[`docs/clustering.md`](clustering.md#the-floor-is-where-the-room-puts-it).
A third of `prog11`'s objects sit under it at any moment, every one of them
already under the relative floor; `cc`'s are silence; nothing on `crowd40`
and `broad40` is under either and nothing changes there. The per-band floor
waits for the perceptual rule to ship.

## 8. Headroom and a limiter

**What is missing.** An element is a sum, and two coherent objects in the same
element exceed full scale. `encode` clamps and counts; it does not prevent.

**The quality wanted.** No sample outside the domain, and a spatial image left
intact when something has to come down.

**Where to look.** The look-ahead limiter is in Giannoulis, Massberg and
Reiss [12] and in Zölzer [11]. Bounded-variable least squares, BVLS, is Stark and
Parker [24], the sibling of the NNLS of Lawson and Hanson [25] already written in
`fit.rs`.

**How to do it our way.**

- **Prevent it in the fit.** `Σ_i w_ik·g_i·peak_i` bounds the peak of element
  `k`. When the bound exceeds the domain, solve the fit again with an upper
  bound on the weights, BVLS. That is what a limiter after the fact cannot do:
  it does not know the weights.
- **Only the residual** goes through a limiter, with a gain **common to every
  element**, because a gain per element moves the image of a spread object and
  the metric would not see it. Offline, the gain is smoothed ahead of the peak,
  not after it.
- Report both: how many blocks were bounded, how many were limited.

**How to judge it.** A scene built to clip, two coherent objects in one element.
Clips counted: zero. The price of the bound in the metric. A level column, since
the metric does not see a limiter's gain.

**Built and measured, the limiter taken and the bound not** (September 2026).
`hz_cluster::headroom` bounds an element's coherent peak in the fit through
a bounded solve, `hz_cluster::mix::Limiter` holds the residual with one gain
for every element ahead of the peak, the scene measures every object's peak,
and `make-master --coherent 2` builds the scene that clips;
`encode --headroom limit|bound|both|off`. The tables are in
[`docs/clustering.md`](clustering.md#no-sample-outside-the-domain). The bound
fires on every block of `crowd40`, costs a fifth of the mean and halves the
clips; the limiter ends them on every scene for nothing the metric sees, and
its price is in the report — the peak before it, the blocks it dips in — and
in the stream, which grows by seven per cent where it dips in every block.
What remains: a bound that knows the phases, which might earn its place.

## 9. Calibrating the metric by ear, once

**What is missing.** A reproducible form for the listening that settles the
ties: flat against K-weighted, nearest against fitted on widths.

**Where to look.** MUSHRA, BS.1534 [26], and BS.1116 [27] for small impairments.

**How to do it our way.** Render on 7.1.4 with the repository's own panner, the
one of Pulkki [28], the original and each rule, on a few scenes chosen where the
metric's verdicts diverge. A MUSHRA ranking. The result fixes the metric's
weighting, once, and the notebook says on what.

**Built, no sheet scored yet** (September 2026). `cargo xtask bench`: the reference,
BS.1534's anchor and each `loudness/weighting` condition rendered on 7.1.4
with the repository's own panner, blinded by a seeded shuffle with a key and
a sheet, and `--score` turning a filled-in sheet into the ranking, with the
hidden-reference check. See
[`docs/clustering.md`](clustering.md#a-bench-for-the-listening-that-settles-a-tie).
The ranking a scored sheet gives is the result this lever is for, and until
one is in every tie above stands on the metric alone, which cannot judge by
ear.

## 10. An element's position is a commitment, not an answer to every block

**What is missing.** Every figure in `docs/clustering.md` is a static error:
how far `Σ_k w_ik·r(q_k)` is from `r(p_i)` at one instant. The metric has no
time axis at all, and the stability counters beside it — travel, jumps,
flips — are diagnostics printed after the fact, not terms the fold minimises.
So a fold that is right at every instant and *different* at every instant
scores perfectly.

That is the fold this encoder writes. Measured on a programme's own master,
decoded back from a delivered stream and folded one element short of its
sources — which is what a re-authored scene asks for, and what no scene in
this tree contains: a rear element re-decided its position on **71.5 %** of
the blocks, by 4.4° a time, and left its place and came back inside a tenth
of a second **244 times over seventy seconds**. The stream the same content
was delivered as moves its rear elements on 7 to 10 % of blocks, by 1.2 to
1.5°, which is one step of the position code: they are *still*, and what
changes is the quantiser's rounding. The mean error over the presentations
was 0.0002 in every one of those folds, ours and the delivered one alike.

The metric cannot see the defect, and every lever measured against it has
therefore measured null on it: the cost of moving from 0.005 to 2.0, the
dominant member as a candidate, the capture seed, and the ownership margin
walked from five degrees to thirty all leave the figure where it was. Lever
2's blocks behind take between a sixth and a third of it, which is the
tremor in the energies showing through and not the shape of the rule.

**Why it is the rear that is reported.** Localisation blur is about a degree
in front and nearer ten at the sides and behind [29]; a *moving* source is
detected far below that, by the minimum audible movement angle, which falls
with the source's velocity and is a few degrees at the velocities a fold
produces [30, 31]. The rear is the one place where a static error of several
degrees is generously forgiven and a wobble of three is not. The metric
measures precisely the quantity the rear forgives and is blind to the one it
does not — which is why the defect a listener reports is at the rear, and why
the figures say nothing is wrong.

**The quality wanted.** An element moves when the *scene* moves, and holds
still when only the *mixture inside it* does.

**Where to look.** The blur that says how much static error a direction
forgives is in Blauert [29]. The threshold that says how little movement is
noticed is the minimum audible movement angle of Perrott and Musicant [30],
against velocity, frequency and azimuth in Chandler and Grantham [31]. That
a fixed set of directions can render a direction between them — so that an
element held still is not an element that has given anything up — is Pulkki
[28] and the reference renderer of BS.2127 [14].

**How to do it our way.**

- **A dead band on the placement, in the metric's own terms.** An element
  keeps the position it had while the fold's cost of keeping it is within a
  margin of the best the search can find: the shape `restart::WORTH` and
  `earn::WORTH` already have, one stage further in. This is **not** the cap
  that was built and rejected — see "Bounding how far an element may move
  makes it move further". A cap clamps an element short of where it should
  be and the next block clamps it again, so one arrival becomes a slide that
  never settles. A dead band either stays where it was or goes the whole
  way; there is no half-move for the next block to compound.
- **The mixture's movement is carried by the weights.** Rendering is linear
  and the fit already spreads an object over as many as three elements, so
  an element held at `q` still reproduces `r(p_i)` when its neighbours supply
  the direction: its position matters through the hull it makes with them
  [28, 14], not on its own. When the mixture inside an element changes and
  the scene has not, the answer is to re-solve the weights — which is done
  every block anyway, at no cost — and leave `q` alone. What the measurement
  has to say is what holding `q` costs.
- **A commitment horizon.** A shared element's position is chosen once per
  span of `restart::RECONSIDER` blocks — the cadence the restart and the
  exchange already run at — as the position that costs least over the whole
  span, with only the weights varying inside it. That is the offline
  encoder's privilege, the same one that pays for the look-ahead.
- **The part that is ours: a ruler that can see it.** Rule 2 of this
  document, and this lever cannot be judged without it. For each element and
  each block, the angle it moved and the time it took are a velocity, and
  the movement is *audible* when it exceeds the minimum audible movement
  angle at that velocity [30, 31]. Sum those, weighted by what the element
  carries and by nothing else. A count of flips or a mean travel cannot
  stand in: both charge the same for an element following a scene across the
  room, which is the fold working, and for one darting out and back, which
  is the fold failing.

**How to judge it.** The audible-movement figure above, on `burst40` and on
a real master folded one element short of its sources — the scene this lever
exists for, which no synthesised scene in the tree contains, so a fourth is
wanted: `make-master --scene contested`, more objects than there are
elements and every one of them wanted. The static error on the old ruler, to
know the price of holding still, and the level column, since a dead band
does not change what an element carries. `prog11` and `cc` unchanged: with
an element for every object there is no mixture to hold. And the listening
of lever 9, because a rule aimed at what a listener notices is a rule a
listener settles.

**Built: the ruler, and nothing else** (September 2026).
`hz_cluster::motion`, reported by `cargo xtask cluster` and by `harlettizer
encode`, on the positions **as the stream carries them** rather than as the
fold holds them. Over each window of the ear's integration time it separates
the *net* — where the element ended up, which is movement the mix asked for —
from the *wobble*, the rest of the path, which went nowhere; and it reports
both against the blur, and again as a rate with no threshold in it. On the
programme this lever was opened for, folded one element short of its sources:

| behind | wobble past the blur | of what is playing | invented | asked |
|---|---|---|---|---|
| **0**, which ships | 3.63 % | 0.12 % | 1.44°/s | 1.39°/s |
| 2 | 2.20 % | 0.07 % | 1.32°/s | 1.37°/s |
| 4 | 1.39 % | 0.04 % | 1.26°/s | 1.36°/s |

Two things it said that were not expected. **More than half the movement in
the stream is movement the mix never asked for** — 1.44°/s invented against
1.39°/s asked — which is the defect stated as a quantity for the first time.
And the fold `cargo xtask cluster` reported for the same master at the same
element count did **not** wobble at all on the same ruler, 0.00 % against the
encoder's 3.63 % — the harness is supposed to be a report about the stream, and
on that master it was not. **Settled** (September 2026), and it was two things
at once: the harness counted the clustered elements where the encoder counts
the bitstream's, so it folded one element wider; and it placed a block with the
state in force at the block's start where the encoder holds the state at the
span's end. Both are in
[`docs/clustering.md`](clustering.md#what-an-element-count-means-and-what-it-meant-before).
The two now agree element for element in 88 % of blocks and to the printed
precision everywhere else, and report the same figures from either side —
3.63 %, 2.20 % and 1.39 % at nought, two and four blocks behind.

The published threshold also has a limit the ruler states in its own
documentation: measured on the fold that was reported, no *single* excursion
passes the blur — they are about three degrees and the rear blur is five and a
half. What is heard is a sustained fluctuation, and the threshold for noticing
that a place will not hold still is not the threshold for telling two places
apart once. No source here puts a number on the first. That is what the rate
with no threshold is for, and what lever 9's listening would calibrate.

**Built and taken: the dead band** (September 2026). `place::HOLDING`, a
quarter: an element keeps the position it had unless the best place the search
can find is better by a quarter of what staying costs *and* by an absolute
floor per unit of what it carries — the shape `restart::WORTH` and
`earn::WORTH` already have, one stage in. Not the cap that was rejected: there
is no half-move for the next block to compound, the element stays exactly
where it was or goes the whole way. On the programme the wobble falls from
3.63 % of the element-windows to **0.84 %**, the share of what is playing that
sits in a wobbling element from 0.12 % to 0.03 %, and the movement the mix
never asked for from 1.44°/s to 1.20°/s, for an error that does not move at
all and four per cent of the clustering's time. A master whose fold is the
identity is untouched.

**And what it costs is not error, it is flips.** An element held in place is
one the fit routes objects through from a slightly stale position, so its
active set changes oftener: 102 flips become 166 on the programme and 22
become 72 on the ring the notebook keeps for exactly this. The band moves
instability out of the positions and some of it into the weights, and the
metric cannot say which a listener would rather have. What it can say is that
the reach behind of lever 2 pays the cost back — the two fight the same tremor
from opposite ends — and that the band with four blocks behind is a
twenty-third of the wobble, 0.16 %, for four more flips than shipping neither
and a worst object that improves. The tables are in `place::HOLDING`.

**What remains of this lever.** The three rules that are not the band: the
mixture's movement carried by the weights, the commitment horizon, and a
threshold for a sustained fluctuation, which is lever 9's. And the trade above,
which is a tie of exactly the kind rule 4 says listening settles.

**Where the figures above come from.** Seventy seconds of one programme, its
own master, folded into eleven elements against the twelve its objects and
bed ask for, and its delivered stream read back for comparison. One
programme is not a measurement, and the first thing this lever owes is the
same figures over the masters to hand.

## Order

1, then 2, then 3, then 4: those are the absent qualities. 5 to 8 are
refinements. **10 before any of the rest is re-measured**: it is the only one
so far whose defect a listener has reported and the metric has scored as
perfect, and its ruler is what would tell the levers already measured null
apart from levers that are simply null. 9 as soon as the listening bench
exists. Dependencies: 1 before 7
for a per-band floor, 3 before 4 for the "bed" class. Lever 1 is built and
measured and did not earn the default; its band analysis is what 7 depends on,
and that is in the tree whichever rule steers. Lever 2 is built and measured
and taken in part: the reach ahead, not the reach behind. Lever 3 is built,
measured and taken, and the "bed" class 4 wants is a mode that snaps. Lever 4
is built, measured and taken. Levers 5 and 6 are built and measured and did
not earn the default. Lever 7 is built, measured and taken, broadband. Lever
8 is built and measured, its limiter taken and its bound not. Lever 9's bench
is built and has scored no sheet yet; the ties of 1, 2 and 5 wait on it.
Lever 10's ruler is built and the first of its rules with it, the dead band; it depends on 9 for its last
word and on nothing else, and the half of lever 2 that was measured and not
taken — the blocks behind — is a partial remedy for it, which `harlettizer
encode --smooth-behind` now makes measurable on a programme rather than on a
scene.

## What is not replayed

Measured winning, with the tables in [`docs/clustering.md`](clustering.md):

- the NNLS over the rendered gain vectors of the four presentations stacked;
- the cap of three paths an object;
- the level put back as rendered;
- the width carried by the fit;
- the Hungarian numbering on what an element carries;
- the sticky active set, and the rejection of the Tikhonov pull;
- the ramp of the weights across the block;
- placement on the metric, and the rejection of the "mean of gain vectors"
  proxy;
- the cold restart with a cost of moving;
- the rejection of a cap on travel a block.

## References

1. ISO 532-1:2017, *Acoustics — Methods for calculating loudness — Part 1: Zwicker method*.
2. ISO 532-2:2017, *Acoustics — Methods for calculating loudness — Part 2: Moore-Glasberg method*.
3. B. C. J. Moore, B. R. Glasberg, T. Baer, "A model for the prediction of thresholds, loudness, and partial loudness", *J. Audio Eng. Soc.* 45(4), 1997.
4. B. R. Glasberg, B. C. J. Moore, "Derivation of auditory filter shapes from notched-noise data", *Hearing Research* 47, 1990.
5. ISO 226:2003, *Acoustics — Normal equal-loudness-level contours*.
6. ITU-R BS.1387-1, *Method for objective measurements of perceived audio quality* (PEAQ).
7. N. Tsingos, E. Gallo, G. Drettakis, "Perceptual audio rendering of complex virtual environments", *ACM Transactions on Graphics* 23(3), SIGGRAPH 2004.
8. A. W. Bronkhorst, "The cocktail party phenomenon: a review of research on speech intelligibility in multiple-talker conditions", *Acta Acustica united with Acustica* 86, 2000.
9. ITU-R BS.1770-4, *Algorithms to measure audio programme loudness and true-peak audio level*.
10. EBU Tech 3341, *Loudness Metering: 'EBU Mode' metering to supplement EBU R 128*.
11. U. Zölzer (ed.), *DAFX: Digital Audio Effects*, 2nd ed., Wiley, 2011.
12. D. Giannoulis, M. Massberg, J. D. Reiss, "Digital dynamic range compressor design — A tutorial and analysis", *J. Audio Eng. Soc.* 60(6), 2012.
13. ITU-R BS.2076-2, *Audio Definition Model*.
14. ITU-R BS.2127-0, *Audio Definition Model renderer for advanced sound systems*.
15. Dolby Laboratories, *Dolby Atmos Master ADM Profile*, public specification. To be used only in its freely downloadable edition, without a click-through agreement.
16. ITU-R BS.2051-2, *Advanced sound system for programme production*.
17. ITU-R BS.2094-1, *Common definitions for the audio definition model*.
18. D. Arthur, S. Vassilvitskii, "k-means++: The advantages of careful seeding", *SODA*, 2007.
19. G. L. Nemhauser, L. A. Wolsey, M. L. Fisher, "An analysis of approximations for maximizing submodular set functions — I", *Mathematical Programming* 14, 1978.
20. L. Kaufman, P. J. Rousseeuw, *Finding Groups in Data: An Introduction to Cluster Analysis*, Wiley, 1990.
21. ATSC A/52:2018, *Digital Audio Compression (AC-3, E-AC-3)*.
22. SMPTE RP 200:2012, *Relative and Absolute Sound Pressure Levels for Motion-Picture Multichannel Sound Systems*.
23. EBU R 128, *Loudness normalisation and permitted maximum level of audio signals*.
24. P. B. Stark, R. L. Parker, "Bounded-variable least-squares: an algorithm and applications", *Computational Statistics* 10, 1995.
25. C. L. Lawson, R. J. Hanson, *Solving Least Squares Problems*, Prentice-Hall, 1974.
26. ITU-R BS.1534-3, *Method for the subjective assessment of intermediate quality level of audio systems* (MUSHRA).
27. ITU-R BS.1116-3, *Methods for the subjective assessment of small impairments in audio systems*.
28. V. Pulkki, "Virtual sound source positioning using vector base amplitude panning", *J. Audio Eng. Soc.* 45(6), 1997.
29. J. Blauert, *Spatial Hearing: The Psychophysics of Human Sound Localization*, revised ed., MIT Press, 1997.
30. D. R. Perrott, A. D. Musicant, "Minimum auditory movement angle: binaural localization of moving sound sources", *J. Acoust. Soc. Am.* 62(6), 1977.
31. D. W. Chandler, D. W. Grantham, "Minimum audible movement angle in the horizontal plane as a function of stimulus frequency and bandwidth, source azimuth, and velocity", *J. Acoust. Soc. Am.* 91(3), 1992.
