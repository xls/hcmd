Name:           hcmd
Version:        @VERSION@
Release:        1%{?dist}
Summary:        A Total Commander alternative for the terminal, for fingers that learned F5 in 1998
License:        MIT
URL:            https://github.com/xls/hcmd

# The binary is built before rpmbuild runs and staged into the buildroot, so
# there is nothing to compile here and no build dependency to declare.
%description
Two panels, the classic function keys, and a viewer that opens a very large file
as fast as a small one. Archives, read-only disk images, SFTP and FTP all browse
as directories. Search runs in process; nothing is shelled out to.

%files
%{_bindir}/hcmd
%{_datadir}/hcmd/examples
%{_datadir}/hcmd/themes
%doc %{_datadir}/doc/hcmd/README.md
%doc %{_datadir}/doc/hcmd/FEATURES.md

%changelog
