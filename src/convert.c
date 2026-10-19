/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * convert.c
 *	  RDF terms as the column types of kb.sparql()'s rows (docs/design.md
 *	  §2.3).
 *
 *	text, varchar, char, name	a literal's lexical form, an IRI, "_:" and a
 *								blank node's label
 *	json, jsonb					the term as SPARQL's JSON results write it:
 *								{"type": "uri" | "bnode" | "literal",
 *								 "value", "datatype", "xml:lang"}
 *	numbers, boolean, dates,	the literal's value through the type's input
 *	times, timestamps			function: XML Schema's lexical forms are the
 *								ISO forms PostgreSQL reads, but for INF and
 *								-INF, a date's time zone, and a negative
 *								duration, adapted here
 *	interval					an xsd:duration
 *	bytea						an xsd:hexBinary or xsd:base64Binary's bytes,
 *								any other value's UTF-8
 *	any other type				its input function over the lexical form
 *
 * An unbound variable is NULL.  A value the type does not take is an error,
 * with the column's name.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include "catalog/pg_type.h"
#include "common/base64.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/json.h"
#include "utils/jsonb.h"
#include "utils/lsyscache.h"
#include "utils/timestamp.h"

#include "pg_knowledge_base.h"

#define XSD "http://www.w3.org/2001/XMLSchema#"

static bool
str_eq(KbStr s, const char *c)
{
	size_t		len = strlen(c);

	return (size_t) s.len == len && memcmp(s.ptr, c, len) == 0;
}

/* The term's text: a literal's lexical form, an IRI, a blank node's "_:label". */
static char *
term_text(const KbTerm *term)
{
	if (term->kind == KB_TERM_BNODE)
		return psprintf("_:%.*s", (int) term->value.len, term->value.ptr);
	return kb_cstring(term->value);
}

/* SPARQL's JSON results form of a term (sparesults/src/json.rs). */
static char *
term_json(const KbTerm *term)
{
	StringInfoData s;

	initStringInfo(&s);
	appendStringInfoString(&s, "{\"type\": ");
	switch (term->kind)
	{
		case KB_TERM_IRI:
			appendStringInfoString(&s, "\"uri\"");
			break;
		case KB_TERM_BNODE:
			appendStringInfoString(&s, "\"bnode\"");
			break;
		default:
			appendStringInfoString(&s, "\"literal\"");
			break;
	}
	appendStringInfoString(&s, ", \"value\": ");
	escape_json_with_len(&s, term->value.ptr ? term->value.ptr : "", (int) term->value.len);
	if (term->kind == KB_TERM_LITERAL)
	{
		if (term->lang.len > 0)
		{
			appendStringInfoString(&s, ", \"xml:lang\": ");
			escape_json_with_len(&s, term->lang.ptr, (int) term->lang.len);
		}
		else if (term->datatype.len > 0 && !str_eq(term->datatype, XSD "string"))
		{
			appendStringInfoString(&s, ", \"datatype\": ");
			escape_json_with_len(&s, term->datatype.ptr, (int) term->datatype.len);
		}
	}
	appendStringInfoChar(&s, '}');
	return s.data;
}

static Datum
input(Oid typid, int32 typmod, char *text)
{
	Oid			typinput;
	Oid			typioparam;

	getTypeInputInfo(typid, &typinput, &typioparam);
	return OidInputFunctionCall(typinput, text, typioparam, typmod);
}

static int
hexval(char c)
{
	if (c >= '0' && c <= '9')
		return c - '0';
	if (c >= 'a' && c <= 'f')
		return c - 'a' + 10;
	if (c >= 'A' && c <= 'F')
		return c - 'A' + 10;
	return -1;
}

static Datum
to_bytea(const KbTerm *term)
{
	bytea	   *out;

	if (term->kind == KB_TERM_LITERAL && str_eq(term->datatype, XSD "hexBinary"))
	{
		int64		i;

		if (term->value.len % 2 != 0)
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_TEXT_REPRESENTATION),
					 errmsg("invalid xsd:hexBinary value")));
		out = palloc(VARHDRSZ + term->value.len / 2);
		SET_VARSIZE(out, VARHDRSZ + term->value.len / 2);
		for (i = 0; i < term->value.len; i += 2)
		{
			int			hi = hexval(term->value.ptr[i]);
			int			lo = hexval(term->value.ptr[i + 1]);

			if (hi < 0 || lo < 0)
				ereport(ERROR,
						(errcode(ERRCODE_INVALID_TEXT_REPRESENTATION),
						 errmsg("invalid xsd:hexBinary value")));
			((unsigned char *) VARDATA(out))[i / 2] = (unsigned char) (hi * 16 + lo);
		}
		return PointerGetDatum(out);
	}
	if (term->kind == KB_TERM_LITERAL && str_eq(term->datatype, XSD "base64Binary"))
	{
		int			maxlen = pg_b64_dec_len((int) term->value.len);
		int			len;

		out = palloc(VARHDRSZ + maxlen);
		len = pg_b64_decode(term->value.ptr, (int) term->value.len,
							(uint8 *) VARDATA(out), maxlen);
		if (len < 0)
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_TEXT_REPRESENTATION),
					 errmsg("invalid xsd:base64Binary value")));
		SET_VARSIZE(out, VARHDRSZ + len);
		return PointerGetDatum(out);
	}
	return PointerGetDatum(cstring_to_text_with_len(term->value.ptr ? term->value.ptr : "",
													(int) term->value.len));
}

Datum
kb_term_datum(const KbTerm *term, Oid typid, int32 typmod, bool *isnull, const char *column)
{
	Oid			base;
	char	   *value;

	if (term->kind == KB_TERM_NONE)
	{
		*isnull = true;
		return (Datum) 0;
	}
	*isnull = false;
	base = getBaseType(typid);
	switch (base)
	{
		case TEXTOID:
			if (base == typid)
				return PointerGetDatum(cstring_to_text(term_text(term)));
			return input(typid, typmod, term_text(term));
		case VARCHAROID:
		case BPCHAROID:
		case NAMEOID:
			return input(typid, typmod, term_text(term));
		case JSONBOID:
		case JSONOID:
			return input(typid, typmod, term_json(term));
		case BYTEAOID:
			return to_bytea(term);
		default:
			break;
	}
	if (term->kind != KB_TERM_LITERAL)
	{
		/* an IRI or a blank node only where the type reads it */
		if (base != UUIDOID && base != TEXTOID)
		{
			value = term_text(term);
			ereport(ERROR,
					(errcode(ERRCODE_DATATYPE_MISMATCH),
					 errmsg("column \"%s\" of type %s cannot hold the %s %s",
							column, format_type_be(typid),
							term->kind == KB_TERM_IRI ? "IRI" : "blank node", value)));
		}
	}
	value = kb_cstring(term->value);
	switch (base)
	{
		case FLOAT4OID:
		case FLOAT8OID:
		case NUMERICOID:
			if (strcmp(value, "INF") == 0 || strcmp(value, "+INF") == 0)
				value = "Infinity";
			else if (strcmp(value, "-INF") == 0)
				value = "-Infinity";
			break;
		case DATEOID:
			{
				/* an xsd:date may end with its time zone, which a date has not */
				char	   *tz = value + strlen(value);

				if (tz > value && tz[-1] == 'Z')
					tz[-1] = '\0';
				else if (strlen(value) > 6 && (value[strlen(value) - 6] == '+' ||
											   value[strlen(value) - 6] == '-') &&
						 value[strlen(value) - 3] == ':')
					value[strlen(value) - 6] = '\0';
				break;
			}
		case INTERVALOID:
			if (value[0] == '-')
			{
				Datum		positive = input(typid, typmod, value + 1);

				return DirectFunctionCall1(interval_um, positive);
			}
			break;
		default:
			break;
	}
	return input(typid, typmod, value);
}

char *
kb_term_json(const KbTerm *term)
{
	return term_json(term);
}
