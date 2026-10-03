{ pkgs }:
pkgs.writeScriptBin "project-docs" ''
  #!${pkgs.python3}/bin/python3 -I
  ${builtins.readFile ../runtime/project_docs.py}
''
