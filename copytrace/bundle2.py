# Copyright 2015 Facebook, Inc.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from mercurial import bundle2, util, exchange, hg, error
from mercurial.i18n import _
import dbutil
import error as dberror


# Temporarily used to force to load the module
def _pullbundle2extraprepare(orig, pullop, kwargs):
    return orig(pullop, kwargs)


def pullmoves(repo, nodelist, source="default"):
    """
    fetches move data from the server
    """
    # Manually creates a pull bundle so as to request mising move data from the
    # server while not pulling possibly present new commits

    source, branches = hg.parseurl(repo.ui.expandpath(source))

    # If n default server defined: abort
    try:
        remote = hg.peer(repo, {}, source)
    except Exception:
        return

    repo.ui.status(_('pulling move data from %s\n') % util.hidepassword(source))
    pullop = exchange.pulloperation(repo, remote, nodelist, False)
    lock = pullop.repo.lock()
    try:
        pullop.trmanager = exchange.transactionmanager(repo, 'pull',
                                                       remote.url())
        _pullmovesbundle2(pullop)
        pullop.trmanager.close()
    finally:
        pullop.trmanager.release()
        lock.release()


def _pullmovesbundle2(pullop):
    """
    fetches move data from the server
    """
    # Creates a bundle with the '000000' commit as common and heads so that no
    # commits are pulled and that this commit exists both on the client and the
    # server
    # Adds the wanted move data in the 'movedatareq' bundle
    kwargs = {}
    kwargs['bundlecaps'] = exchange.caps20to10(pullop.repo)
    kwargs['movedatareq'] = pullop.heads
    kwargs['common'] = [pullop.repo[-1].node()]
    kwargs['heads'] = [pullop.repo[-1].node()]
    kwargs['cg'] = False
    bundle = pullop.remote.getbundle('pull', **kwargs)
    try:
        op = bundle2.processbundle(pullop.repo, bundle, pullop.gettransaction)
    except error.BundleValueError as exc:
        raise error.Abort('missing support for %s' % exc)


@exchange.b2partsgenerator('push:movedata')
def _pushb2movedata(pushop, bundler):
    """
    adds a part containing the movedata when pushing new commits -- client-side
    """
    repo = pushop.repo

    try:
        # Send the whole database
        if repo.ui.configbool('copytrace', 'pushmvdb'):
            dic = dbutil.retrieveallrawdata(repo)
        # Send the moves related to the pushed commits
        else:
            ctxlist = _processctxlist(repo, pushop.remoteheads, pushop.revs)
            # Moves to send
            if ctxlist:
                dic = dbutil.retrieverawdata(repo, ctxlist)
            # No moves to send
            else:
                return
    except Exception as e:
        dberror.logfailure(repo, e, "_pushb2movedata")
        return

    data = _encodedict(dic)
    repo.ui.status('moves for %d changesets pushed\n' % len(dic.keys()))

    part = bundler.newpart('push:movedata', data=data, mandatory=False)


@bundle2.parthandler('push:movedata')
def _handlemovedatarequest(op, inpart):
    """
    processes the part containing the movedata during a push -- server-side
    """
    dic = _decodedict(inpart)
    op.records.add('movedata', {'mvdict': dic})

    try:
        # Retrieves the hashes modified by push-rebase to modify the raw move
        # data with the correct hashes
        mapping = op.records['b2x:rebase']
        if not mapping:
            mapping = {}
        else:
            mapping = mapping[0]
        dbutil.insertrawdata(op.repo, dic, mapping)
    except Exception as e:
        dberror.logfailure(op.repo, e, "_handlemovedatarequest-push")


@exchange.getbundle2partsgenerator('pull:movedata')
def _getbundlemovedata(bundler, repo, source, bundlecaps=None, heads=None,
                       common=None,  b2caps=None, **kwargs):
    """
    adds the part containing the movedata requested by a pull -- server-side
    """
    # Retrieves the manually requested ctx during a 'fake' pull
    ctxlist = kwargs.get('movedatareq', [])
    # Adds the ctx which are actually pulled
    ctxlist.extend(_processctxlist(repo, common, heads))
    # List of ctx for which move data is requested
    if ctxlist:
        try:
            dic = dbutil.retrieverawdata(repo, ctxlist)
        except Exception as e:
            dberror.logfailure(repo, e, "_getbundlemovedata")
            return
        data = _encodedict(dic)

        part = bundler.newpart('pull:movedata', data=data, mandatory=False)


@bundle2.parthandler('pull:movedata')
def _handlemovedatarequest(op, inpart):
    """
    processes the part containing the movedata during a pull -- client-side
    """
    dic = _decodedict(inpart)
    op.records.add('movedata', {'mvdict': dic})
    op.repo.ui.warn('moves for %d changesets retrieved\n' % len(dic.keys()))
    try:
        dbutil.insertrawdata(op.repo, dic)
    except Exception as e:
        dberror.logfailure(op.repo, e, "_handlemovedatarequest-pull")


def _processctxlist(repo, remoteheads, localheads):
    """
    processes the ctx list between remoteheads and localheads
    """

    if not localheads:
        localheads = [repo[rev].node() for rev in repo.changelog.headrevs()]
    if not remoteheads:
        remoteheads = []

    return [ctx.hex() for ctx in
            repo.set("only(%ln, %ln)", localheads, remoteheads)]


def _encodedict(dic):
    """
    encodes the content of the move data for exchange over the wire
    dic = {ctxhash: [(src, dst, mv)]}
    """
    expandedlist = []
    for ctxhash, mvlist in dic.iteritems():
        for src, dst, mv in mvlist:
             expandedlist.append('%s\t%s\t%s\t%s' % (ctxhash, src, dst, mv))
    return '\n'.join(expandedlist)


def _decodedict(data):
    """
    decodes the content of the move data from exchange over the wire
    """
    result = {}
    for l in data.read().splitlines():
        ctxhash, src, dst, mv = l.split('\t')
        result.setdefault(ctxhash, []).append((src, dst, mv))
    return result
