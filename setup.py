from glob import glob

try:
    from setuptools import setup, Extensions
except ImportError:
    from distutils.core import setup, Extension

setup(
    name='fbhgext',
    version='0.1.2',
    author='Durham Goode',
    maintainer='Durham Goode',
    maintainer_email='durham@fb.com',
    url='',
    description='Facebook specific mercurial extensions',
    long_description="",
    keywords='fb hg mercurial',
    license='',
    packages=[
        'fastmanifest',
        'phabricator',
        'sqldirstate',
    ],
    py_modules=[
        p[:-3].replace('/', '.')
        for p in glob('hgext3rd/*.py')
        if p != 'hgext3rd/__init__.py'
    ],
    ext_modules = [
        Extension('cfastmanifest',
                  sources=['cfastmanifest.c',
                           'cfastmanifest/bsearch.c',
                           'cfastmanifest/buffer.c',
                           'cfastmanifest/checksum.c',
                           'cfastmanifest/node.c',
                           'cfastmanifest/tree.c',
                           'cfastmanifest/tree_arena.c',
                           'cfastmanifest/tree_convert.c',
                           'cfastmanifest/tree_copy.c',
                           'cfastmanifest/tree_diff.c',
                           'cfastmanifest/tree_disk.c',
                           'cfastmanifest/tree_iterator.c',
                           'cfastmanifest/tree_path.c',
                  ],
                  include_dirs=['cfastmanifest',
                                '/usr/local/include',
                                '/opt/local/include',
                  ],
                  library_dirs=[
                      '/usr/local/lib',
                      '/opt/local/lib',
                  ],
                  libraries=['crypto',
                  ],
                  extra_compile_args=[
                      "-std=c99",
                      "-Wall",
                      "-Werror", "-Werror=strict-prototypes"],
        )
    ],
)
