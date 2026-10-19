# SPDX-License-Identifier: Apache-2.0
"""The bench leg's dataset (docs/design.md §6): products, their offers and
reviews, in the shape of the Berlin SPARQL Benchmark's e-commerce data --
types in a hierarchy, features, producers, vendors, reviewers -- made from a
seed, so that each run of a scale has the same triples, as N-Triples.

    generate.py PRODUCTS OUT.nt

About 74 triples a product: its own 21, four offers of 8, two reviews of
10, and a quarter of a reviewer.  Labels are of a few words (some fit the
store's inline strings, some do not), comments and review texts long, in
English or German, and the literals of the datatypes oxigraph keeps as
values: integers, decimals, dates, dateTimes.
"""
import random
import sys

NS = "http://example.com/bench/"
V = NS + "vocab#"
RDF = "http://www.w3.org/1999/02/22-rdf-syntax-ns#"
RDFS = "http://www.w3.org/2000/01/rdf-schema#"
FOAF = "http://xmlns.com/foaf/0.1/"
DC = "http://purl.org/dc/elements/1.1/"
XSD = "http://www.w3.org/2001/XMLSchema#"

WORDS = """able acid aged also area army away baby back ball band bank base bath bear beat
been beer bell belt best bill bird blow blue boat body bomb bond bone book boom born boss
both bowl bulk burn bush busy call calm came camp card care case cash cast cell chat chip
city club coal coat code cold come cook cool cope copy core cost crew crop dark data date
dawn days dead deal dear debt deep deny desk dial diet disc disk does done door dose down
draw drew drop drug dual duke dust duty each earn ease east easy edge else even ever evil
exit face fact fail fair fall farm fast fate fear feed feel feet fell felt file fill film
find fine fire firm fish five flat flow food foot ford form fort four free from fuel full
fund gain game gate gave gear gene gift girl give glad goal goes gold golf gone good gray
grew grey grow gulf hair half hall hand hang hard harm hate have head hear heat held hell
help here hero high hill hire hold hole holy home hope host hour huge hung hunt hurt idea
inch into iron item jack jane jean john join jump jury just keen keep kent kept kick kill
kind king knee knew know lack lady laid lake land lane last late lead left less life lift
like line link list live load loan lock logo long look lord lose loss lost love luck made
mail main make male many mark mass matt meal mean meat meet menu mere mike mile milk mill
mind mine miss mode mood moon more most move much must name navy near neck need news next
nice nick nine none nose note okay once only onto open oral over pace pack page paid pain
pair palm park part pass past path peak pick pink pipe plan play plot plug plus poll pool
poor port post pull pure push race rail rain rank rare rate read real rear rely rent rest
rice rich ride ring rise risk road rock role roll roof room root rose rule rush ruth safe
über straße größe zebra""".split()

COUNTRIES = ["AT", "BE", "CH", "CN", "CZ", "DE", "DK", "ES", "FI", "FR",
             "GB", "IE", "IN", "IT", "JP", "NL", "NO", "PL", "SE", "US"]


def iri(local):
    return f"<{NS}{local}>"


def v(local):
    return f"<{V}{local}>"


def lit(text, lang=None):
    text = text.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{text}"@{lang}' if lang else f'"{text}"'


def typed(value, datatype):
    return f'"{value}"^^<{XSD}{datatype}>'


class Out:
    def __init__(self, f):
        self.f = f
        self.n = 0

    def t(self, s, p, o):
        self.f.write(f"{s} {p} {o} .\n")
        self.n += 1


def words(rng, a, b):
    return " ".join(rng.choice(WORDS) for _ in range(rng.randint(a, b)))


def generate(products, path, seed=20261019):
    rng = random.Random(seed)
    a = f"<{RDF}type>"
    label = f"<{RDFS}label>"
    comment = f"<{RDFS}comment>"
    with open(path, "w", encoding="utf-8") as f:
        out = Out(f)
        # the types: a root, 6, 30, 120 leaves
        types = {0: None}
        level = [0]
        n = 1
        for width in (6, 5, 4):
            nxt = []
            for parent in level:
                for _ in range(width):
                    types[n] = parent
                    nxt.append(n)
                    n += 1
            level = nxt
        leaves = level
        for t, parent in types.items():
            out.t(iri(f"type/{t}"), a, f"<{RDFS}Class>")
            out.t(iri(f"type/{t}"), label, lit(f"type {t} {words(rng, 1, 2)}"))
            if parent is not None:
                out.t(iri(f"type/{t}"), f"<{RDFS}subClassOf>", iri(f"type/{parent}"))
        nfeatures = 1000
        for i in range(nfeatures):
            out.t(iri(f"feature/{i}"), a, v("Feature"))
            out.t(iri(f"feature/{i}"), label, lit(f"feature {words(rng, 1, 2)}"))
        nproducers = max(1, products // 200)
        nvendors = max(1, products // 100)
        npersons = max(1, products // 4)
        for i in range(nproducers):
            s = iri(f"producer/{i}")
            out.t(s, a, v("Producer"))
            out.t(s, label, lit(f"{words(rng, 1, 2)} inc"))
            out.t(s, comment, lit(words(rng, 20, 40)))
            out.t(s, v("country"), iri(f"country/{rng.choice(COUNTRIES)}"))
            out.t(s, f"<{FOAF}homepage>", f"<http://producer{i}.example.com/>")
        for i in range(nvendors):
            s = iri(f"vendor/{i}")
            out.t(s, a, v("Vendor"))
            out.t(s, label, lit(f"{words(rng, 1, 2)} shop"))
            out.t(s, comment, lit(words(rng, 20, 40)))
            out.t(s, v("country"), iri(f"country/{rng.choice(COUNTRIES)}"))
            out.t(s, f"<{FOAF}homepage>", f"<http://vendor{i}.example.com/>")
        for i in range(npersons):
            s = iri(f"person/{i}")
            out.t(s, a, f"<{FOAF}Person>")
            out.t(s, f"<{FOAF}name>", lit(f"{rng.choice(WORDS).title()} {rng.choice(WORDS).title()}"))
            out.t(s, f"<{FOAF}mbox_sha1sum>", lit("%040x" % rng.getrandbits(160)))
            out.t(s, v("country"), iri(f"country/{rng.choice(COUNTRIES)}"))
        offer = review = 0
        reviewed = None  # a product of the middle with reviews, for the queries
        for i in range(products):
            s = iri(f"product/{i}")
            producer = iri(f"producer/{rng.randrange(nproducers)}")
            out.t(s, a, iri(f"type/{rng.choice(leaves)}"))
            out.t(s, label, lit(words(rng, 1, 3)))
            out.t(s, comment, lit(words(rng, 20, 50)))
            out.t(s, v("producer"), producer)
            out.t(s, f"<{DC}publisher>", producer)
            for feature in rng.sample(range(nfeatures), rng.randint(5, 15)):
                out.t(s, v("productFeature"), iri(f"feature/{feature}"))
            out.t(s, v("numeric1"), typed(rng.randint(1, 2000), "integer"))
            out.t(s, v("numeric2"), typed(rng.randint(1, 2000), "integer"))
            if rng.random() < 0.8:
                out.t(s, v("numeric3"), typed(rng.randint(1, 2000), "integer"))
            out.t(s, v("textual1"), lit(words(rng, 3, 6)))
            out.t(s, v("textual2"), lit(words(rng, 3, 6)))
            out.t(s, f"<{DC}date>",
                  typed(f"{rng.randint(2000, 2008)}-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}", "date"))
            for _ in range(4):
                o = iri(f"offer/{offer}")
                offer += 1
                out.t(o, a, v("Offer"))
                out.t(o, v("product"), s)
                out.t(o, v("vendor"), iri(f"vendor/{rng.randrange(nvendors)}"))
                out.t(o, v("price"), typed(f"{rng.randint(5, 10000)}.{rng.randint(0, 99):02d}", "decimal"))
                start = (rng.randint(2006, 2008), rng.randint(1, 12), rng.randint(1, 28))
                out.t(o, v("validFrom"), typed("%04d-%02d-%02dT00:00:00" % start, "dateTime"))
                out.t(o, v("validTo"), typed("%04d-%02d-%02dT00:00:00" % ((start[0] + 1,) + start[1:]), "dateTime"))
                out.t(o, v("deliveryDays"), typed(rng.randint(1, 21), "integer"))
                out.t(o, v("offerWebpage"), f"<http://vendor.example.com/offer/{offer}>")
            nreviews = rng.choice((0, 1, 2, 2, 3, 4))
            if reviewed is None and i >= products // 3 and nreviews >= 2:
                reviewed = i
            for _ in range(nreviews):
                r = iri(f"review/{review}")
                review += 1
                out.t(r, a, v("Review"))
                out.t(r, v("reviewFor"), s)
                out.t(r, v("reviewer"), iri(f"person/{rng.randrange(npersons)}"))
                out.t(r, f"<{DC}title>", lit(words(rng, 3, 7)))
                out.t(r, v("text"), lit(words(rng, 40, 80), rng.choice(("en", "en", "de"))))
                out.t(r, v("reviewDate"),
                      typed(f"{rng.randint(2005, 2008)}-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}", "date"))
                for k in range(1, 5):
                    if rng.random() < 0.7:
                        out.t(r, v(f"rating{k}"), typed(rng.randint(1, 10), "integer"))
    return {"products": products, "offers": offer, "reviews": review, "persons": npersons,
            "vendors": nvendors, "producers": nproducers, "triples": out.n,
            "reviewed_product": reviewed if reviewed is not None else 0}


if __name__ == "__main__":
    print(generate(int(sys.argv[1]), sys.argv[2]))
