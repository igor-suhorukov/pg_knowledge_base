# SPDX-License-Identifier: Apache-2.0
# pg_knowledge_base (docs/design.md §2.1): the extension, built by PGXS.  The
# engine library is built apart, by test/engine.sh in the builder image, and
# installed beside the module as libkb_engine.so.

MODULE_big = pg_knowledge_base
OBJS = \
	src/call.o \
	src/convert.o \
	src/engine.o \
	src/functions.o \
	src/module.o \
	src/sql.o \
	src/store.o \
	src/trace.o \
	src/endpoint/acceptor.o \
	src/endpoint/hba.o \
	src/endpoint/http.o \
	src/endpoint/login.o \
	src/endpoint/session.o \
	src/endpoint/settings.o \
	src/endpoint/tls.o

EXTENSION = pg_knowledge_base
DATA = sql/pg_knowledge_base--0.1.sql
PGFILEDESC = "pg_knowledge_base - oxigraph's SPARQL engine, an RDF store, R2RML and a SPARQL endpoint"
PG_CPPFLAGS = -I$(srcdir)/include -I$(srcdir)/src
SHLIB_LINK = -ldl -lssl -lcrypto

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The engine library, from the builder's output (test/run.sh engine)
KB_ENGINE ?= /engine/libkb_engine.so
install-engine:
	$(INSTALL_SHLIB) $(KB_ENGINE) '$(DESTDIR)$(pkglibdir)/libkb_engine.so'
.PHONY: install-engine
