# Each curated option's own declared metadata, in one evaluation.
#
# One expression rather than one evaluation per key, because starting the
# evaluator is the expensive part and the module system is built either way.
#
# Every field is guarded so one awkward option cannot take the whole catalog
# down. A default or example may legitimately be a function, a derivation, or
# something that throws, and a description may be an attribute set rather than a
# string in older nixpkgs. Anything that is not plain data becomes null, which
# the catalog renders as "not declared".
{ dir, optionsPath, keys }:

let
  opts = getPath (builtins.getFlake dir) (segs optionsPath);

  segs = s: builtins.filter builtins.isString (builtins.split "\\." s);

  getPath = builtins.foldl'
    (acc: name: if acc != null && builtins.isAttrs acc && acc ? ${name} then acc.${name} else null);

  # A recursive type check rather than a toJSON attempt: tryEval catches only
  # throw and assert, so a type error such as "cannot convert a function to
  # JSON" escapes it and would abort the whole evaluation. Derivations are
  # excluded by their outPath, since forcing one is expensive and a store path
  # is not what a catalog reader wants.
  jsonable = v:
    let t = builtins.typeOf v; in
    if t == "int" || t == "bool" || t == "string" || t == "float" || t == "null" then true
    else if t == "list" then builtins.all jsonable v
    else if t == "set" then !(v ? outPath) && builtins.all jsonable (builtins.attrValues v)
    else false;

  # An option whose default genuinely is null is indistinguishable here from one
  # whose default could not be read. Accepted.
  safe = v:
    let r = builtins.tryEval (if jsonable v then v else null);
    in if r.success then r.value else null;

  str = v: let s = safe v; in if builtins.isString s then s else null;

  meta = path:
    let
      r = builtins.tryEval (getPath opts (segs path));
      o = if r.success then r.value else null;
    in
    if o == null then null else {
      type_name = str (o.type.description or null);
      default = safe (o.default or null);
      description = str (o.description or null);
      example = safe (o.example or null);
    };
in
builtins.listToAttrs (map (k: { name = k; value = meta k; }) keys)
