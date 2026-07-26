/// The one rule for identifiers that enter a signed string.
///
/// `^[A-Za-z0-9_.:-]{1,64}$`. The characters left out are the point: the
/// canonical string joins fields with `|`, so an id containing one could move
/// the boundaries and let two different frames share a signature. `:` is in
/// because RPC stream ids are `rpc:{uuid}`.
library;

final RegExp _idPattern = RegExp(r'^[A-Za-z0-9_.:-]{1,64}$');

bool isValidId(String id) => _idPattern.hasMatch(id);
